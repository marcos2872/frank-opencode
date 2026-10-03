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

#[allow(clippy::too_many_arguments)]
pub(crate) async fn forward_anthropic(
    s: &AppState,
    headers: &HeaderMap,
    body: &mut Value,
    entry: &CatalogEntry,
    base: &str,
    bearer: &secrecy::SecretString,
    gateway_model: &str,
    stream: bool,
    variant: Option<&str>,
) -> Response {
    body["model"] = Value::String(entry.model_id.clone());
    // Apply the selected variant to the raw body: the Messages API forwards
    // client fields verbatim, so `thinking`/`include` land unchanged. A
    // variant the Messages API cannot represent is a 400, not a silent no-op.
    if let Some(v) = variant {
        match apply_variant_checked(std::mem::take(body), entry, v) {
            Ok(applied) => *body = applied,
            Err(e) => {
                return gateway_error_response(&GatewayError::VariantNotRepresentable {
                    detail: e.to_string(),
                })
            }
        }
    }
    apply_entry_body(body, entry);
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
    for (k, v) in entry_headers(entry) {
        if k.eq_ignore_ascii_case("anthropic-beta") {
            continue; // handled above (merged).
        }
        req = req.header(k, v);
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            return gateway_error_response(&GatewayError::UpstreamUnreachable {
                source: e.to_string(),
            })
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let summary = request_summary(body);
        log_upstream_error(entry, gateway_model, base, status, &text, &summary);
        return upstream_error_response(status, &text);
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
                return gateway_error_response(&GatewayError::InvalidUpstreamJson {
                    source: e.to_string(),
                })
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
pub(crate) async fn forward_responses(
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
    let mut resp_body = if let Some(v) = variant {
        apply_variant(anthropic_to_responses(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_responses(body, &entry.model_id)
    };
    apply_entry_body(&mut resp_body, entry);
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
    for (k, v) in entry_headers(entry) {
        req = req.header(k, v);
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&resp_body).send().await {
        Ok(r) => r,
        Err(e) => {
            return gateway_error_response(&GatewayError::UpstreamUnreachable {
                source: e.to_string(),
            })
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let summary = request_summary(body);
        log_upstream_error(entry, gateway_model, base, status, &text, &summary);
        return upstream_error_response(status, &text);
    }
    if !stream {
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return gateway_error_response(&GatewayError::InvalidUpstreamJson {
                    source: e.to_string(),
                })
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
        let mut input_tokens: u64 = 0;
        let mut incomplete = false;
        let mut upstream_error: Option<String> = None;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    upstream_error = Some(format!("upstream stream read failed: {e}"));
                    break;
                }
            };
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
                    if let Some(u) = r.get("usage") {
                        if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                            input_tokens = n;
                            tr.input_tokens = n;
                        }
                        if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
                            output_tokens = n;
                        }
                    }
                }
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("response.incomplete") => incomplete = true,
                    // The upstream failed after the stream opened: surface it
                    // rather than ending 200 with no explanation.
                    Some("response.failed") => {
                        upstream_error = Some(response_failure_message(&v));
                    }
                    _ => {}
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
        for line in tr.finish(reason, input_tokens, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = upstream_error {
            let err = sse_error("api_error", &msg);
            yield Ok::<_, std::io::Error>(err.into_bytes());
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
pub(crate) async fn forward_openai(
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
    let mut oai_body = if let Some(v) = variant {
        apply_variant(anthropic_to_openai(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_openai(body, &entry.model_id)
    };
    apply_entry_body(&mut oai_body, entry);
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
    for (k, v) in entry_headers(entry) {
        req = req.header(k, v);
    }
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    let resp = match req.json(&oai_body).send().await {
        Ok(r) => r,
        Err(e) => {
            return gateway_error_response(&GatewayError::UpstreamUnreachable {
                source: e.to_string(),
            })
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let summary = request_summary(body);
        log_upstream_error(entry, gateway_model, base, status, &text, &summary);
        return upstream_error_response(status, &text);
    }
    if !stream {
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return gateway_error_response(&GatewayError::InvalidUpstreamJson {
                    source: e.to_string(),
                })
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
        let mut input_tokens: u64 = 0;
        let mut stop_reason = "end_turn".to_string();
        let mut upstream_error: Option<String> = None;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    upstream_error = Some(format!("upstream stream read failed: {e}"));
                    break;
                }
            };
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
                if let Some(u) = v.get("usage") {
                    if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
                        input_tokens = n;
                        tr.input_tokens = n;
                    }
                    if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
                        output_tokens = n;
                    }
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
        if tr.tool_blocks.iter().any(|b| b.started) {
            stop_reason = "tool_use".to_string();
        }
        for line in tr.finish(&stop_reason, input_tokens, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = upstream_error {
            let err = sse_error("api_error", &msg);
            yield Ok::<_, std::io::Error>(err.into_bytes());
        }
    };
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(with_heartbeat(out, HEARTBEAT_IDLE)))
        .unwrap()
}

/// Compatibility surface: `main.rs` and the test suites import
/// `AppState`, `router` and the boot constants from `api::server`.
/// They now live in `state` / `routes`; re-exported here so no
/// external path changes in Fase 4.
pub use crate::api::routes::router;
pub use crate::api::state::{
    AppState, BOOT_CATALOG_ATTEMPTS, BOOT_CATALOG_BACKOFF, BOOT_CATALOG_MAX_BACKOFF,
    HEARTBEAT_IDLE,
};
