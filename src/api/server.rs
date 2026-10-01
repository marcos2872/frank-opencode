//! HTTP layer: Axum handlers. Business decisions live in domain/infra.

use crate::config::AppConfig;
use crate::domain::{
    auto_alias, is_known_package, protocol_for, strip_window_suffix, AliasEntry, CatalogEntry,
    ModelRef, Protocol,
};
use crate::infra::opencode::{fetch_catalog, upstream_bearer, CredentialStore};
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, estimate_tokens, join_url,
    openai_to_anthropic, responses_to_anthropic, sse, with_heartbeat, ResponsesTranslator,
    StreamTranslator,
};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use secrecy::ExposeSecret;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// Idle gap after which translated/passthrough SSE streams emit `ping`
/// (keeps Claude Code's stream watchdog fed during long reasoning pauses).
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub catalog: Arc<RwLock<Vec<CatalogEntry>>>,
    pub aliases: Arc<RwLock<Vec<AliasEntry>>>,
    pub last_refresh: Arc<RwLock<Option<SystemTime>>>,
    pub last_error: Arc<RwLock<Option<String>>>,
    pub http: reqwest::Client,
    pub db_path: PathBuf,
}

impl AppState {
    pub fn new(config: AppConfig, db_path: PathBuf) -> Self {
        Self {
            config,
            catalog: Arc::new(RwLock::new(vec![])),
            aliases: Arc::new(RwLock::new(vec![])),
            last_refresh: Arc::new(RwLock::new(None)),
            last_error: Arc::new(RwLock::new(None)),
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
    /// Failures keep the previous catalog and are recorded for `/health`.
    pub async fn refresh(&self) -> Result<usize, String> {
        let bin = self.config.opencode_bin.clone();
        let fetch = tokio::task::spawn_blocking(move || fetch_catalog(&bin));
        let entries = match tokio::time::timeout(Duration::from_secs(20), fetch).await {
            Ok(Ok(Ok(entries))) => entries,
            Ok(Ok(Err(e))) => {
                let msg = format!("catalog fetch failed: {e}");
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
            Ok(Err(e)) => {
                let msg = format!("catalog task failed: {e}");
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
            Err(_) => {
                let msg = "catalog fetch timed out after 20s".to_string();
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
        };
        for e in &entries {
            tracing::debug!(
                provider_id = %e.provider_id,
                model_id = %e.model_id,
                package = %e.package,
                "catalog entry"
            );
            if !is_known_package(&e.package) {
                tracing::warn!(
                    provider_id = %e.provider_id,
                    package = %e.package,
                    "unknown provider package, using ChatCompletions fallback"
                );
            }
        }
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
        *self.last_refresh.write().await = Some(SystemTime::now());
        *self.last_error.write().await = None;
        Ok(n)
    }

    /// Single source of truth for the default model (config or first alias).
    pub async fn effective_default(&self) -> String {
        if !self.config.default_model.is_empty() {
            return self.config.default_model.clone();
        }
        self.aliases
            .read()
            .await
            .first()
            .map(|a| a.gateway_id.clone())
            .unwrap_or_default()
    }

    /// Resolve a requested model (gateway alias, `provider/model`, plain id,
    /// each optionally with a `#variant` suffix) to its catalog entry and the
    /// selected variant label. `Err` carries the 404 message.
    async fn resolve(&self, requested: &str) -> Result<(CatalogEntry, Option<String>), String> {
        let (base, variant) = match requested.split_once('#') {
            Some((b, v)) => (b.to_string(), Some(v.to_string())),
            None => (requested.to_string(), None),
        };
        let hit = self.lookup_entry(&base).await;
        self.finish_resolve(hit, &base, variant).await
    }

    /// Catalog lookup under the read guards (kept out of `resolve` so the
    /// variant checks in `finish_resolve` don't hold any lock).
    async fn lookup_entry(&self, base: &str) -> Option<CatalogEntry> {
        let catalog = self.catalog.read().await;
        let aliases = self.aliases.read().await;
        // Alias hit?
        if let Some(a) = aliases.iter().find(|a| a.gateway_id == base) {
            if let Some(r) = ModelRef::parse(&a.opencode_ref) {
                let hit = catalog
                    .iter()
                    .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                    .cloned();
                if hit.is_some() {
                    return hit;
                }
            }
        }
        // Direct provider/model?
        if let Some(r) = ModelRef::parse(base) {
            if let Some(hit) = catalog
                .iter()
                .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                .cloned()
            {
                return Some(hit);
            }
        }
        // Plain model id (first enabled match)? Sort for determinism
        // (ambiguity is order-dependent across providers) and warn.
        let mut hits: Vec<CatalogEntry> = catalog
            .iter()
            .filter(|e| e.model_id == base)
            .cloned()
            .collect();
        hits.sort_by_key(|e| e.qualified());
        if hits.len() > 1 {
            tracing::warn!(
                model = base,
                candidates = ?hits.iter().map(|e| e.qualified()).collect::<Vec<_>>(),
                "ambiguous model id, using first; prefer an alias or provider/model"
            );
        }
        hits.into_iter().next()
    }

    async fn finish_resolve(
        &self,
        entry: Option<CatalogEntry>,
        base: &str,
        variant: Option<String>,
    ) -> Result<(CatalogEntry, Option<String>), String> {
        let Some(entry) = entry else {
            return Err(format!("unknown model '{base}' (see GET /v1/models)"));
        };
        let v = match variant {
            None => None,
            Some(v) => {
                let known: Vec<&str> = entry.variants.iter().map(|x| x.id.as_str()).collect();
                if known.iter().any(|x| **x == v) {
                    Some(v)
                } else if known.is_empty() {
                    return Err(format!(
                        "model '{}' has no variants; requested '{}'",
                        entry.qualified(),
                        v
                    ));
                } else {
                    return Err(format!(
                        "model '{}' has no variant '{}' (available: {})",
                        entry.qualified(),
                        v,
                        known.join(", ")
                    ));
                }
            }
        };
        Ok((entry, v))
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
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Gateway credential check. `/health` stays open (daemon probes + `enable`
/// waiter are unauthenticated); everything else requires the configured
/// `auth_token` in either `x-api-key` or `Authorization: Bearer` when set.
async fn require_token(
    State(s): State<AppState>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if req.uri().path() == "/health" || s.config.auth_token.is_empty() {
        return next.run(req).await;
    }
    let headers = req.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if bearer == s.config.auth_token || key == s.config.auth_token {
        next.run(req).await
    } else {
        anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid gateway credential (check ANTHROPIC_AUTH_TOKEN)",
        )
    }
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    // Short-lived locks only: `effective_default()` takes `aliases` itself,
    // so don't hold that guard across the call.
    let model_count = s.aliases.read().await.len();
    let last_error = s.last_error.read().await.clone();
    let refreshed = s.last_refresh.read().await.is_some();
    let default_model = s.effective_default().await;
    let status = if last_error.is_some() {
        "degraded"
    } else if refreshed {
        "ok"
    } else {
        "starting"
    };
    Json(serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "default_model": default_model,
        "models": model_count,
        "last_error": last_error,
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
    let _ = crate::daemon::write_private(path, &id);
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

/// `POST /v1/messages/count_tokens`: validate like the Messages endpoint,
/// then return the Anthropic count when the upstream package is Anthropic
/// (proxy), else the local per-part estimate. The upstream count_tokens
/// endpoint exists only in the Anthropic wire protocol.
async fn count_tokens(State(s): State<AppState>, Json(body): Json<Value>) -> Response {
    // Model resolution first: same 404/400 semantics as /v1/messages.
    let raw = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let requested = if raw.is_empty() {
        s.effective_default().await
    } else {
        strip_window_suffix(&raw).to_string()
    };
    if requested.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let (entry, _variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(msg) => return anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg),
    };
    // A count without messages is meaningless.
    if body.get("messages").and_then(|m| m.as_array()).is_none() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing messages",
        );
    }

    if let Some(base) = entry.base_url() {
        if protocol_for(&entry.package) == Protocol::Anthropic {
            if let Some(n) = proxy_count_tokens(&s, base, &entry, &body).await {
                return Json(serde_json::json!({"input_tokens": n})).into_response();
            }
            // Proxy failed: fall through to the estimate.
        }
    }
    let n = estimate_tokens(&body);
    Json(serde_json::json!({"input_tokens": n})).into_response()
}

/// Ask `{baseURL}/messages/count_tokens` and return the token count.
/// Fallback to `None` on any transport/parse error so callers degrade to the
/// local estimate (the endpoint is optional in this gateway's contract).
async fn proxy_count_tokens(
    s: &AppState,
    base: &str,
    entry: &CatalogEntry,
    body: &Value,
) -> Option<u64> {
    let store = CredentialStore::new(s.db_path.clone());
    let bearer = upstream_bearer(entry, &store)?;
    let mut req_body = body.clone();
    // The upstream counts under its own model id, not the gateway alias.
    req_body["model"] = Value::String(entry.model_id.clone());
    let url = join_url(base, "messages/count_tokens");
    let resp = s
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .json(&req_body)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("input_tokens").and_then(|n| n.as_u64())
}

async fn messages(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    let raw = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    // Accept Claude Code's `[1m]`/`[200k]` window hints on unknown gateway ids.
    let requested = if raw.is_empty() {
        s.effective_default().await
    } else {
        strip_window_suffix(&raw).to_string()
    };
    if requested.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let (entry, variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(msg) => {
            return anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg);
        }
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

    match protocol_for(&entry.package) {
        Protocol::Anthropic => {
            forward_anthropic(
                &s,
                &headers,
                &mut body,
                &entry,
                &base,
                &bearer,
                &requested,
                stream,
                variant.as_deref(),
            )
            .await
        }
        Protocol::Responses => {
            forward_responses(
                &s,
                &headers,
                &body,
                &entry,
                &base,
                &bearer,
                &requested,
                stream,
                variant.as_deref(),
            )
            .await
        }
        Protocol::ChatCompletions => {
            forward_openai(
                &s,
                &headers,
                &body,
                &entry,
                &base,
                &bearer,
                &requested,
                stream,
                variant.as_deref(),
            )
            .await
        }
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
    _variant: Option<&str>,
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
        // Byte passthrough, but inject `ping` during upstream silence so the
        // client's stream watchdog doesn't abort long thinking pauses.
        let stream = with_heartbeat(
            resp.bytes_stream()
                .map(|c| c.map(|b| b.to_vec()).map_err(std::io::Error::other)),
            HEARTBEAT_IDLE,
        );
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
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
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
    variant: Option<&str>,
) -> Response {
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let resp_body = if let Some(v) = variant {
        apply_variant(anthropic_to_responses(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_responses(body, &entry.model_id)
    };
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
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
        }
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
        .body(Body::from_stream(with_heartbeat(out, HEARTBEAT_IDLE)))
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
    variant: Option<&str>,
) -> Response {
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let oai_body = if let Some(v) = variant {
        apply_variant(anthropic_to_openai(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_openai(body, &entry.model_id)
    };
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
        if let Some(uid) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_str())
        {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
        }
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
        .body(Body::from_stream(with_heartbeat(out, HEARTBEAT_IDLE)))
        .unwrap()
}
