//! HTTP layer: Axum handlers. Business decisions live in domain/infra.

use crate::config::AppConfig;
use crate::domain::{auto_alias, AliasEntry, CatalogEntry, ModelRef};
use crate::infra::opencode::{fetch_catalog, upstream_bearer, CredentialStore};
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, estimate_tokens, join_url, openai_to_anthropic,
    responses_to_anthropic, sse, ResponsesTranslator, StreamTranslator,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use secrecy::ExposeSecret;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub catalog: Arc<RwLock<Vec<CatalogEntry>>>,
    pub aliases: Arc<RwLock<Vec<AliasEntry>>>,
    pub http: reqwest::Client,
    pub db_path: PathBuf,
}

impl AppState {
    pub fn new(config: AppConfig, db_path: PathBuf) -> Self {
        Self {
            config,
            catalog: Arc::new(RwLock::new(vec![])),
            aliases: Arc::new(RwLock::new(vec![])),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .user_agent(format!("frank-opencode/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("http client"),
            db_path,
        }
    }

    /// Rebuild catalog + alias map. Manual aliases win; disabled are skipped.
    /// Console free-tier (`opencode/*`) models are skipped unless
    /// `include_free_tier` is set: they 403 outside OpenCode.
    pub async fn refresh(&self) -> Result<usize, String> {
        let entries = fetch_catalog(&self.config.opencode_bin)?;
        let usable: Vec<&CatalogEntry> = entries
            .iter()
            .filter(|e| self.config.include_free_tier || e.provider_id != "opencode")
            .collect();
        let mut aliases: Vec<AliasEntry> = vec![];
        let mut used_refs = std::collections::HashSet::new();

        // 1. Manual aliases first.
        let mut manual: Vec<AliasEntry> = self
            .config
            .aliases
            .iter()
            .map(|(gw, a)| AliasEntry {
                gateway_id: gw.clone(),
                opencode_ref: a.opencode.clone(),
                display_name: a.display_name.clone().unwrap_or_else(|| gw.clone()),
                description: a
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("via frank-opencode · {}", a.opencode)),
            })
            .collect();
        manual.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
        for a in manual {
            if self.config.is_disabled(&a.opencode_ref, &a.gateway_id) {
                continue;
            }
            used_refs.insert(a.opencode_ref.clone());
            aliases.push(a);
        }
        // 2. Auto aliases for the rest (free-tier excluded unless opted in).
        let mut auto: Vec<AliasEntry> = usable
            .iter()
            .filter(|e| !used_refs.contains(&e.qualified()))
            .map(|e| auto_alias(e))
            .filter(|a| !self.config.is_disabled(&a.opencode_ref, &a.gateway_id))
            .collect();
        auto.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
        // Avoid gateway_id collisions with manual entries.
        let taken: std::collections::HashSet<String> =
            aliases.iter().map(|a| a.gateway_id.clone()).collect();
        for a in auto {
            if !taken.contains(&a.gateway_id) {
                aliases.push(a);
            }
        }
        aliases.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));

        let n = usable.len();
        *self.catalog.write().await = entries;
        *self.aliases.write().await = aliases;
        Ok(n)
    }

    async fn resolve(&self, requested: &str) -> Option<CatalogEntry> {
        let catalog = self.catalog.read().await;
        let aliases = self.aliases.read().await;
        // Alias hit?
        if let Some(a) = aliases.iter().find(|a| a.gateway_id == requested) {
            if let Some(r) = ModelRef::parse(&a.opencode_ref) {
                return catalog
                    .iter()
                    .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                    .cloned();
            }
        }
        // Direct provider/model?
        if let Some(r) = ModelRef::parse(requested) {
            if let Some(hit) = catalog
                .iter()
                .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                .cloned()
            {
                return Some(hit);
            }
        }
        // Plain model id (first enabled match)?
        catalog.iter().find(|e| e.model_id == requested).cloned()
    }
}

fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": err_type, "message": msg}});
    (status, Json(body)).into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .with_state(state)
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    let aliases = s.aliases.read().await;
    let default = if s.config.default_model.is_empty() {
        aliases
            .first()
            .map(|a| a.gateway_id.clone())
            .unwrap_or_default()
    } else {
        s.config.default_model.clone()
    };
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "default_model": default,
        "models": aliases.len(),
    }))
}

async fn list_models(State(s): State<AppState>) -> impl IntoResponse {
    let aliases = s.aliases.read().await;
    let data: Vec<Value> = aliases
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.gateway_id,
                "display_name": a.display_name,
                "description": a.description,
                "owned_by": "frank-opencode",
            })
        })
        .collect();
    Json(serde_json::json!({"object": "list", "data": data}))
}

use serde_json::Value;

/// Stable fallback session id (persisted) for clients that send no session
/// header (e.g. curl smoke tests). Go uses it for routing/prompt caching.
fn fallback_session_id() -> String {
    let path = AppConfig::data_dir().join("frank.session");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let id = format!("frank-{}", uuid::Uuid::new_v4());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    id
}

/// Outgoing Go session headers derived from the incoming client headers.
/// Go recognizes Claude Code's native session header; always also send
/// `x-opencode-session` (required for routing).
fn session_headers(incoming: &HeaderMap) -> Vec<(String, String)> {
    let mut out = vec![];
    let claude = incoming
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let direct = incoming
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !claude.is_empty() {
        out.push(("x-claude-code-session-id".to_string(), claude.clone()));
    }
    let session = if !direct.is_empty() {
        direct
    } else if !claude.is_empty() {
        claude
    } else {
        fallback_session_id()
    };
    out.push(("x-opencode-session".to_string(), session));
    out
}

async fn count_tokens(State(_s): State<AppState>, Json(body): Json<Value>) -> impl IntoResponse {
    // Optional endpoint: Claude Code falls back to char estimate when absent.
    // We implement the estimate server-side.
    let n = estimate_tokens(&body);
    Json(serde_json::json!({"input_tokens": n}))
}

async fn messages(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let requested = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let requested = if requested.is_empty() {
        s.config.default_model.clone()
    } else {
        requested
    };
    if requested.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let Some(entry) = s.resolve(&requested).await else {
        return anthropic_error(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("unknown model '{requested}' (see GET /v1/models)"),
        );
    };

    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let store = CredentialStore::new(s.db_path.clone());
    let Some(bearer) = upstream_bearer(&entry, &store) else {
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            &format!(
                "no stored credential for '{}' (run `opencode auth login`)",
                entry.provider_id
            ),
        );
    };

    let Some(base) = entry.base_url().map(|x| x.to_string()) else {
        return anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("provider '{}' has no baseURL", entry.provider_id),
        );
    };

    if entry.is_anthropic_package() {
        forward_anthropic(
            &s, &headers, &mut body, &entry, &base, &bearer, &requested, stream,
        )
        .await
    } else if entry.is_responses_package() {
        forward_responses(
            &s, &headers, &body, &entry, &base, &bearer, &requested, stream,
        )
        .await
    } else {
        forward_openai(
            &s, &headers, &body, &entry, &base, &bearer, &requested, stream,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_anthropic(
    s: &AppState,
    headers: &HeaderMap,
    body: &mut Value,
    entry: &CatalogEntry,
    base: &str,
    bearer: &secrecy::SecretString,
    gateway_model: &str,
    stream: bool,
) -> Response {
    body["model"] = Value::String(entry.model_id.clone());
    // `stream` passthrough stays as the client sent it.
    let url = join_url(base, "messages");
    let mut req = s
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .header("x-api-key", bearer.expose_secret().to_string())
        .header(
            "anthropic-version",
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("2023-06-01"),
        );
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        req = req.header("anthropic-beta", beta);
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream unreachable: {e}"),
            )
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return (status, [("content-type", "application/json")], text).into_response();
    }
    if stream {
        let stream = resp
            .bytes_stream()
            .map(|c| c.map_err(std::io::Error::other));
        Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        // Rewrite `model` to the gateway id for a consistent client view.
        let mut v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("invalid upstream JSON: {e}"),
                )
            }
        };
        if v.get("model").is_some() {
            v["model"] = Value::String(gateway_model.to_string());
        }
        Json(v).into_response()
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_responses(
    s: &AppState,
    headers: &HeaderMap,
    body: &Value,
    entry: &CatalogEntry,
    base: &str,
    bearer: &secrecy::SecretString,
    gateway_model: &str,
    stream: bool,
) -> Response {
    let resp_body = anthropic_to_responses(body, &entry.model_id);
    let url = join_url(base, "responses");
    let mut req = s
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .header("x-api-key", bearer.expose_secret().to_string());
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&resp_body).send().await {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream unreachable: {e}"),
            )
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return (status, [("content-type", "application/json")], text).into_response();
    }
    if !stream {
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("invalid upstream JSON: {e}"),
                )
            }
        };
        return Json(responses_to_anthropic(&v, gateway_model)).into_response();
    }
    // Translated streaming: Responses SSE -> Anthropic SSE.
    let gw = gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = ResponsesTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        let mut pinned = Box::pin(byte_stream);
        let mut output_tokens: u64 = 0;
        let mut incomplete = false;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk { Ok(b) => b, Err(_) => break };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let t = text.trim();
                let payload = t.strip_prefix("data:").map(|x| x.trim()).unwrap_or("");
                if payload.is_empty() || payload == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(payload) else { continue; };
                // Usage + status arrive on response.completed.
                if let Some(r) = v.get("response") {
                    if let Some(u) = r.get("usage").and_then(|u| u.get("output_tokens")).and_then(|x| x.as_u64()) {
                        output_tokens = u;
                    }
                }
                if v.get("type").and_then(|t| t.as_str()) == Some("response.incomplete") {
                    incomplete = true;
                }
                for ev in tr.feed(&v) {
                    yield Ok(ev.into_bytes());
                }
            }
        }
        if !tr.text_open && !tr.has_tools() {
            let start = sse(&serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
            yield Ok(start.into_bytes());
            tr.text_open = true;
        }
        let reason = if tr.has_tools() {
            "tool_use"
        } else if incomplete {
            "max_tokens"
        } else {
            "end_turn"
        };
        for line in tr.finish(reason, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
    };
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(out))
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn forward_openai(
    s: &AppState,
    headers: &HeaderMap,
    body: &Value,
    entry: &CatalogEntry,
    base: &str,
    bearer: &secrecy::SecretString,
    gateway_model: &str,
    stream: bool,
) -> Response {
    let oai_body = anthropic_to_openai(body, &entry.model_id);
    let url = join_url(base, "chat/completions");
    let mut req = s
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        );
    // Some OpenAI-compatible gateways also accept api-key header; harmless to send.
    req = req.header("x-api-key", bearer.expose_secret().to_string());
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&oai_body).send().await {
        Ok(r) => r,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream unreachable: {e}"),
            )
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return (status, [("content-type", "application/json")], text).into_response();
    }
    if !stream {
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    &format!("invalid upstream JSON: {e}"),
                )
            }
        };
        return Json(openai_to_anthropic(&v, gateway_model)).into_response();
    }
    // Translated streaming: OpenAI SSE -> Anthropic SSE.
    let gw = gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = StreamTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        use futures::StreamExt;
        let mut pinned = Box::pin(byte_stream);
        let mut output_tokens: u64 = 0;
        let mut stop_reason = "end_turn".to_string();
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk { Ok(b) => b, Err(_) => break };
            buf.extend_from_slice(&bytes);
            // Process complete lines.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let t = text.trim();
                let payload = t.strip_prefix("data:").map(|x| x.trim()).unwrap_or("");
                if payload.is_empty() { continue; }
                if payload == "[DONE]" { continue; }
                let Ok(v) = serde_json::from_str::<Value>(payload) else { continue; };
                if let Some(u) = v.get("usage").and_then(|u| u.get("completion_tokens")).and_then(|x| x.as_u64()) {
                    output_tokens = u;
                }
                if let Some(fr) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|c| c.get("finish_reason")).and_then(|f| f.as_str()) {
                    stop_reason = match fr {
                        "tool_calls" => "tool_use".to_string(),
                        "length" => "max_tokens".to_string(),
                        _ => "end_turn".to_string(),
                    };
                }
                // tool_calls finish without content still needs block emission (handled in feed).
                for ev in tr.feed(&v) {
                    yield Ok(ev.into_bytes());
                }
            }
        }
        // If the upstream never opened a text block but we have no content,
        // open/close an empty one so the stream is well-formed.
        if !tr.text_open && tr.tool_blocks.iter().all(|b| !b.started) {
            // Emit empty text block so Claude Code doesn't see an empty stream.
            let start = sse(&serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}));
            yield Ok(start.into_bytes());
            tr.text_open = true;
        }
        for line in tr.finish(&stop_reason, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
    };
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(out))
        .unwrap()
}
