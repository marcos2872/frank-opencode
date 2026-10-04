//! HTTP layer: Axum routes and handlers. Business decisions live in
//! domain/infra; state, forwards, mock, count_tokens, session and error
//! shaping live in the sibling modules, re-exported here for the public path
//! (`api::server::{router, AppState, ...}`).

use super::count_tokens::count_tokens;
use super::errors::anthropic_error;
use super::forward::{forward_anthropic, forward_openai, forward_responses, ForwardCtx};
use super::mock::mock_check_response;
pub use super::state::AppState;
use crate::domain::{protocol_for_entry, strip_window_suffix, window_suffix, Protocol};
use crate::infra::opencode::{upstream_bearer, CredentialStore};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;

pub use super::forward::HEARTBEAT_IDLE;
pub use super::state::{BOOT_CATALOG_ATTEMPTS, BOOT_CATALOG_BACKOFF, BOOT_CATALOG_MAX_BACKOFF};

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
            // Anthropic family tier from `[tiers]` config. Claude Desktop's
            // `small_fast` background class (session titles) picks the first
            // `haiku` model; without a tier it falls back to id substring
            // matching and lands on the first `*sonnet*` row. `is_family_default`
            // is only honored by the Desktop together with a tier, so both
            // are emitted (or neither) for the flagged alias.
            if let Some(tier) = &a.family_tier {
                item["anthropic_family_tier"] = Value::String(tier.clone());
                if a.family_default {
                    item["is_family_default"] = Value::Bool(true);
                }
            }
            item
        })
        .collect();
    Json(serde_json::json!({"object": "list", "data": data}))
}

async fn messages(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
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
        Err(msg) => {
            return anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg);
        }
    };

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

    match protocol_for_entry(&entry) {
        Protocol::Anthropic => {
            forward_anthropic(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
        Protocol::Responses => {
            forward_responses(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
        Protocol::ChatCompletions => {
            forward_openai(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
    }
}
