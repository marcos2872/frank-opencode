//! `POST /v1/messages/count_tokens`: proxy the Anthropic upstream when the
//! entry speaks the Messages API, else estimate locally.

use super::errors::anthropic_error;
use super::forward::{
    apply_entry_body, upstream_post, with_entry_headers_except, with_session_headers,
};
use super::state::AppState;
use crate::domain::{protocol_for_entry, strip_window_suffix, CatalogEntry, Protocol};
use crate::infra::opencode::{upstream_bearer, CredentialStore};
use crate::infra::upstream::{apply_variant_checked, estimate_tokens, join_url};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

/// `POST /v1/messages/count_tokens`: validate like the Messages endpoint,
/// then return the Anthropic count when the upstream package is Anthropic
/// (proxy), else the local per-part estimate. The upstream count_tokens
/// endpoint exists only in the Anthropic wire protocol.
pub(crate) async fn count_tokens(
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
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let (entry, variant) = match s.resolve(&requested).await {
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
    // Client-only `anthropic-beta` here (no catalog merge: unlike the
    // Messages forward, the count proxy never merges fast-mode betas).
    let mut req = upstream_post(s, &url, &bearer).header(
        "anthropic-version",
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("2023-06-01"),
    );
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        req = req.header("anthropic-beta", beta);
    }
    let req = with_session_headers(
        with_entry_headers_except(req, entry, &["anthropic-beta"]),
        headers,
    );
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
