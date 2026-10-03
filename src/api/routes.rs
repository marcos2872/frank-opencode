//! HTTP routes: router, middleware and the two message handlers.
//! The three `forward_*` protocol paths still live in `server.rs`
//! (they move to `forward.rs` in Fase 5).

use crate::api::errors::{anthropic_error, gateway_error_response};
use crate::api::headers::{apply_entry_body, entry_headers, session_headers};
use crate::api::mock::mock_check_response;
use crate::api::server::{forward_anthropic, forward_openai, forward_responses};
use crate::api::state::AppState;
use crate::domain::{
    protocol_for_entry, strip_window_suffix, window_suffix, GatewayError, Protocol,
};
use crate::infra::opencode::{upstream_bearer, CredentialStore};
use crate::infra::upstream::{apply_variant_checked, estimate_tokens, join_url};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use secrecy::ExposeSecret;
use serde_json::Value;

/// Max JSON body accepted on message endpoints (64 MiB).
const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
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
            let mut item = serde_json::json!({
                "id": a.gateway_id,
                "display_name": a.display_name,
                "description": a.description,
                "owned_by": "frank-opencode",
            });
            // Context window from the OpenCode catalog (`limit.context`).
            // Omitted when unknown so clients fall back to their default.
            if let Some(w) = a.context_window {
                item["context_window"] = Value::from(w);
                // Mainline Claude Code reads a window only from the literal
                // `[1m]` suffix on the id (never arbitrary `[<n>k]`), so only
                // windows >= 1M get a suffix. `resolve()` strips it before
                // matching, so the internal gateway_id is untouched.
                if let Some(sfx) = window_suffix(w) {
                    item["id"] = Value::String(format!("{a}{sfx}", a = a.gateway_id));
                }
            }
            item
        })
        .collect();
    Json(serde_json::json!({"object": "list", "data": data}))
}

/// `POST /v1/messages/count_tokens`: validate like the Messages endpoint,
/// then return the Anthropic count when the upstream package is Anthropic
/// (proxy), else the local per-part estimate. The upstream count_tokens
/// endpoint exists only in the Anthropic wire protocol.
async fn count_tokens(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
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
        return gateway_error_response(&GatewayError::MissingModel);
    }
    let (entry, variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(e) => return gateway_error_response(&e),
    };
    // A count without messages is meaningless.
    if body.get("messages").and_then(|m| m.as_array()).is_none() {
        return gateway_error_response(&GatewayError::MissingMessages);
    }

    if let Some(base) = entry.base_url() {
        if protocol_for_entry(&entry) == Protocol::Anthropic {
            if let Some(n) =
                proxy_count_tokens(&s, &headers, base, &entry, &body, variant.as_deref()).await
            {
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
    headers: &HeaderMap,
    base: &str,
    entry: &CatalogEntry,
    body: &Value,
    variant: Option<&str>,
) -> Option<u64> {
    let store = CredentialStore::new(s.db_path.clone());
    let bearer = upstream_bearer(entry, &store)?;
    let mut req_body = body.clone();
    // The upstream counts under its own model id, not the gateway alias.
    req_body["model"] = Value::String(entry.model_id.clone());
    let url = join_url(base, "messages/count_tokens");
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
    for (k, v) in entry_headers(entry) {
        if !k.eq_ignore_ascii_case("anthropic-beta") {
            req = req.header(k, v);
        }
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    req_body.as_object_mut().map(|o| o.remove("stream"));
    // Apply the selected variant and the catalog body defaults, so the count
    // matches what the real forward would send (thinking changes the input
    // token count on the Messages API).
    if let Some(v) = variant {
        match apply_variant_checked(std::mem::take(&mut req_body), entry, v) {
            Ok(applied) => req_body = applied,
            Err(_) => return None,
        }
    }
    apply_entry_body(&mut req_body, entry);
    let resp = req.json(&req_body).send().await.ok()?;
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
        return gateway_error_response(&GatewayError::MissingModel);
    }
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Local mock for Claude Code's auto-mode safety classifier (and its
    // liveness probes): answer here so the check never reaches an upstream
    // that may be out of quota. Intercepts before resolve, so it works for
    // any id; real conversations carry tools and are not matched.
    if s.config.mock_classifier && !stream {
        if let Some(mocked) = mock_check_response(&body, &requested) {
            tracing::info!(model = %requested, "mocked classifier/probe request");
            return Json(mocked).into_response();
        }
    }
    let (entry, variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(e) => {
            return gateway_error_response(&e);
        }
    };

    let store = CredentialStore::new(s.db_path.clone());
    let Some(bearer) = upstream_bearer(&entry, &store) else {
        return gateway_error_response(&GatewayError::NoCredential {
            provider: entry.provider_id.clone(),
        });
    };

    let Some(base) = entry.base_url().map(|x| x.to_string()) else {
        return gateway_error_response(&GatewayError::NoBaseUrl {
            provider: entry.provider_id.clone(),
        });
    };

    match protocol_for_entry(&entry) {
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

/// Truncate an upstream error body for logs (never log credentials here;
///
/// callers only pass status + body, never the bearer).
/// Message from a Responses `response.failed` event: the upstream reports the
