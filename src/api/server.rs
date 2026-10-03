//! Protocol forwards: Anthropic passthrough, Responses and Chat.
//! Temporary home in Fase 4 — Fase 5 unifies these behind one
//! request builder + one SSE pump in `forward.rs` + `infra/upstream`.

use crate::api::errors::{gateway_error_response, log_upstream_error, request_summary,
    response_failure_message, upstream_error_response};
use crate::api::headers::{apply_entry_body, entry_beta_header, entry_headers, session_headers};
use crate::api::state::{AppState, HEARTBEAT_IDLE};
use crate::domain::{CatalogEntry, GatewayError};
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, apply_variant_checked,
    join_url, openai_to_anthropic, responses_to_anthropic, sse, sse_error,
    with_heartbeat, ResponsesTranslator, StreamTranslator,
};
use axum::{body::Body, http::{HeaderMap, StatusCode}, response::Response, Json};
use futures::StreamExt;
use secrecy::ExposeSecret;
use serde_json::Value;

/// Compatibility surface: `main.rs` and the test suites import
/// `AppState`, `router` and the boot constants from `api::server`.
/// They now live in `state` / `routes`; re-exported here so no
/// external path changes across Fases 4-5.
pub use crate::api::forward::{forward_anthropic, forward_openai, forward_responses};
pub use crate::api::routes::router;
pub use crate::api::state::{
    AppState, BOOT_CATALOG_ATTEMPTS, BOOT_CATALOG_BACKOFF, BOOT_CATALOG_MAX_BACKOFF,
    HEARTBEAT_IDLE,
};
