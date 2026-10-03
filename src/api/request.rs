//! Single request builder for all three upstream wire protocols.
//!
//! The only intentional divergences between protocols:
//! - Anthropic merges the client `anthropic-beta` with the catalog default;
//!   translated protocols forward catalog headers verbatim.
//! - Anthropic sends `anthropic-version`.
//! Everything else (bearer, session headers, send, status mapping,
//! error normalization) is identical.

use crate::api::errors::{gateway_error_response, log_upstream_error, request_summary};
use crate::api::errors::upstream_error_response;
use crate::api::headers::{entry_beta_header, entry_headers, session_headers};
use crate::api::state::AppState;
use crate::domain::{CatalogEntry, GatewayError};
use crate::infra::upstream::join_url;
use axum::{
    http::{HeaderMap, StatusCode},
    response::Response,
};
use secrecy::ExposeSecret;
use serde_json::Value;

/// Which wire protocol (and path) this request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireTarget {
    /// `POST {base}/messages` with Anthropic headers.
    Anthropic,
    /// `POST {base}/responses` (translated body).
    Responses,
    /// `POST {base}/chat/completions` (translated body).
    Chat,
}

impl WireTarget {
    fn path(self) -> &'static str {
        match self {
            Self::Anthropic => "messages",
            Self::Responses => "responses",
            Self::Chat => "chat/completions",
        }
    }
}

/// POST the (already translated + variant-applied) body upstream and return
/// the response, or the wire error response on transport failure.
pub async fn post_upstream(
    s: &AppState,
    headers: &HeaderMap,
    entry: &CatalogEntry,
    base: &str,
    bearer: &secrecy::SecretString,
    target: WireTarget,
    body: &Value,
) -> Result<reqwest::Response, Response> {
    let url = join_url(base, target.path());
    let mut req = s
        .http
        .post(&url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .header("x-api-key", bearer.expose_secret().to_string());
    if target == WireTarget::Anthropic {
        req = req.header(
            "anthropic-version",
            headers
                .get("anthropic-version")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("2023-06-01"),
        );
        if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            // Merge client betas with catalog defaults (fast-mode): both are
            // comma-separated lists.
            let merged = match entry_beta_header(entry) {
                Some(catalog) if !catalog.is_empty() && !beta.contains(&catalog) => {
                    format!("{beta}, {catalog}")
                }
                _ => beta.to_string(),
            };
            req = req.header("anthropic-beta", merged);
        } else if let Some(catalog) = entry_beta_header(entry) {
            req = req.header("anthropic-beta", catalog);
        }
    }
    for (k, v) in entry_headers(entry) {
        if target == WireTarget::Anthropic && k.eq_ignore_ascii_case("anthropic-beta") {
            continue; // handled above (merged).
        }
        req = req.header(k, v);
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    req.json(body).send().await.map_err(|e| {
        gateway_error_response(&GatewayError::UpstreamUnreachable {
            source: e.to_string(),
        })
    })
}

/// Map a non-2xx upstream response to the Anthropic error shape, logging the
/// privacy-safe summary. `Ok(resp)` on success, `Err(wire error)` otherwise.
pub async fn check_status(
    resp: reqwest::Response,
    entry: &CatalogEntry,
    gateway_model: &str,
    base: &str,
    client_body: &Value,
) -> Result<reqwest::Response, Response> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().await.unwrap_or_default();
    let summary = request_summary(client_body);
    log_upstream_error(entry, gateway_model, base, status, &text, &summary);
    Err(upstream_error_response(status, &text))
}

/// Parse a successful non-streaming upstream body as JSON, or the wire
/// error on invalid JSON.
pub async fn parse_json_body(resp: reqwest::Response) -> Result<Value, Response> {
    resp.json().await.map_err(|e| {
        gateway_error_response(&GatewayError::InvalidUpstreamJson {
            source: e.to_string(),
        })
    })
}

/// SSE response shell shared by passthrough and translated streams.
pub fn sse_response(
    stream: impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static,
) -> Response {
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}
