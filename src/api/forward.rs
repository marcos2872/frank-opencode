//! Upstream forwards, one per wire protocol, over the shared plumbing below
//! so auth headers, error mapping and SSE framing change in one place.
//! Protocol-specific headers (Anthropic version/beta) stay in their forward.

use super::errors::{
    anthropic_error, log_upstream_error, request_summary, response_failure_message,
    upstream_error_response,
};
use super::session::session_headers;
use super::state::AppState;
use crate::domain::CatalogEntry;
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, apply_variant_checked, join_url,
    openai_to_anthropic, responses_to_anthropic, sse, sse_error, with_heartbeat,
    ResponsesTranslator, StreamTranslator,
};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use secrecy::ExposeSecret;
use serde_json::Value;
use std::time::Duration;

pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(20);

/// Catalog default headers for an entry, excluding auth (bearer is set from
/// the credential store). Keys are matched case-insensitively by callers.
pub(crate) fn entry_headers(entry: &CatalogEntry) -> Vec<(String, String)> {
    let Some(h) = entry.headers.as_ref() else {
        return vec![];
    };
    h.iter()
        .filter(|(k, _)| {
            let l = k.to_ascii_lowercase();
            l != "authorization" && l != "x-api-key"
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub(crate) fn entry_beta_header(entry: &CatalogEntry) -> Option<String> {
    entry.headers.as_ref().and_then(|h| {
        h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.clone())
    })
}

/// Merge catalog `body` defaults (e.g. `{"speed":"fast"}`) into the upstream
/// JSON. Catalog wins on key conflicts: these are model defaults the Go
/// backend would have applied.
pub(crate) fn apply_entry_body(target: &mut Value, entry: &CatalogEntry) {
    let (Some(t), Some(e)) = (
        target.as_object_mut(),
        entry.body.as_ref().and_then(|v| v.as_object()),
    ) else {
        return;
    };
    for (k, v) in e {
        t.insert(k.clone(), v.clone());
    }
}

/// Shared upstream plumbing for the `forward_*` functions and
/// `proxy_count_tokens`, so a change to auth headers, error mapping or SSE
/// framing is made once instead of N times. Protocol-specific headers
/// (Anthropic version/beta) stay in their forward; everything identical across
/// paths lives here.
///
/// Base POST every upstream forward starts from: content-type plus the
/// bearer credential (both header shapes the backends accept).
pub(crate) fn upstream_post(
    s: &AppState,
    url: &str,
    bearer: &secrecy::SecretString,
) -> reqwest::RequestBuilder {
    s.http
        .post(url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .header("x-api-key", bearer.expose_secret().to_string())
}

/// Catalog default headers for this entry (auth excluded: the bearer above
/// is the credential), skipping any header in `skip` (case-insensitive).
/// The Anthropic paths handle `anthropic-beta` themselves (client/catalog
/// merge in the forward, client-only in the count_tokens proxy) and skip it
/// here; the translated forwards pass no skips.
pub(crate) fn with_entry_headers_except(
    req: reqwest::RequestBuilder,
    entry: &CatalogEntry,
    skip: &[&str],
) -> reqwest::RequestBuilder {
    let mut req = req;
    for (k, v) in entry_headers(entry) {
        if skip.iter().any(|s| k.eq_ignore_ascii_case(s)) {
            continue;
        }
        req = req.header(k, v);
    }
    req
}

/// Catalog default headers for this entry (auth excluded: the bearer above
/// is the credential).
pub(crate) fn with_entry_headers(
    req: reqwest::RequestBuilder,
    entry: &CatalogEntry,
) -> reqwest::RequestBuilder {
    with_entry_headers_except(req, entry, &[])
}

/// Session routing headers for this client (always sent).
pub(crate) fn with_session_headers(
    req: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut req = req;
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    req
}

/// POST a JSON body; a transport failure is already the gateway error
/// response (`upstream unreachable`), so callers just early-return it.
/// The error is boxed: an inline `Response` would trip
/// `clippy::result_large_err` (a `Response<Body>` is a large variant).
async fn send_json(
    req: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::Response, Box<Response>> {
    match req.json(body).send().await {
        Ok(r) => Ok(r),
        Err(e) => Err(Box::new(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("upstream unreachable: {e}"),
        ))),
    }
}

/// A non-2xx upstream status: log the privacy-safe summary and return the
/// normalized Anthropic error shape. `summary_body` is whatever the caller
/// logged before (the Anthropic forward logs its mutated body, the translated
/// forwards log the original client body).
fn log_and_map_upstream_error(
    entry: &CatalogEntry,
    gateway_model: &str,
    base: &str,
    status: StatusCode,
    text: String,
    summary_body: &Value,
    headers: &HeaderMap,
) -> Response {
    let summary = request_summary(summary_body);
    log_upstream_error(entry, gateway_model, base, status, &text, &summary, headers);
    upstream_error_response(status, &text)
}

/// Read an upstream JSON body; invalid JSON is already the gateway error.
/// The error is boxed, same as `send_json` above.
async fn read_upstream_json(resp: reqwest::Response) -> Result<Value, Box<Response>> {
    match resp.json().await {
        Ok(v) => Ok(v),
        Err(e) => Err(Box::new(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("invalid upstream JSON: {e}"),
        ))),
    }
}

/// Drain complete lines from the SSE byte buffer, returning the `data:`
/// payloads. Empty lines, non-`data:` lines and `[DONE]` are skipped, exactly
/// as each forward did inline before.
fn drain_sse_payloads(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = vec![];
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line);
        let t = text.trim();
        let payload = t.strip_prefix("data:").map(|x| x.trim()).unwrap_or("");
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        out.push(payload.to_string());
    }
    out
}

/// The empty text-block start both translated streams emit when the upstream
/// never opened any content, so the stream stays well-formed.
fn empty_text_block_event() -> String {
    sse(
        &serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    )
}

/// Terminal error event both translated streams emit when the upstream fails
/// after the stream opened, instead of ending 200 with no explanation.
fn terminal_error_event(msg: &str) -> String {
    sse_error("api_error", msg)
}

/// Final SSE response every streaming forward returns: 200 with heartbeat
/// pings feeding the client's stream watchdog.
fn sse_stream_response<S>(out: S) -> Response
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static,
{
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(with_heartbeat(out, HEARTBEAT_IDLE)))
        .unwrap()
}

/// Shared context for the three protocol `forward_*` functions: everything
/// `messages()` resolved that a forward needs. A struct (not 9 positional
/// args) so a new field doesn't become a tenth parameter.
pub(crate) struct ForwardCtx<'a> {
    pub(crate) s: &'a AppState,
    pub(crate) headers: &'a HeaderMap,
    pub(crate) body: &'a Value,
    pub(crate) entry: &'a CatalogEntry,
    pub(crate) base: &'a str,
    pub(crate) bearer: &'a secrecy::SecretString,
    pub(crate) gateway_model: &'a str,
    pub(crate) stream: bool,
    pub(crate) variant: Option<&'a str>,
}

pub(crate) async fn forward_anthropic(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // The Messages API forwards client fields verbatim, so work on a copy:
    // nothing after the forward reads the caller's body back.
    let mut body = body.clone();
    let body = &mut body;
    body["model"] = Value::String(entry.model_id.clone());
    // Apply the selected variant to the raw body: the Messages API forwards
    // client fields verbatim, so `thinking`/`include` land unchanged. A
    // variant the Messages API cannot represent is a 400, not a silent no-op.
    if let Some(v) = variant {
        match apply_variant_checked(std::mem::take(body), entry, v) {
            Ok(applied) => *body = applied,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &e.to_string(),
                )
            }
        }
    }
    apply_entry_body(body, entry);
    // `stream` passthrough stays as the client sent it.
    let url = join_url(base, "messages");
    // Only the Messages API merges catalog betas into the client's
    // `anthropic-beta` (fast-mode): both are comma-separated lists.
    let mut req = upstream_post(s, &url, bearer).header(
        "anthropic-version",
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("2023-06-01"),
    );
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
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
    let req = with_session_headers(
        with_entry_headers_except(req, entry, &["anthropic-beta"]),
        headers,
    );
    let resp = match send_json(req, body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if stream {
        // Byte passthrough, but inject `ping` during upstream silence so the
        // client's stream watchdog doesn't abort long thinking pauses.
        let stream = with_heartbeat(
            resp.bytes_stream()
                .map(|c| c.map(|b| b.to_vec()).map_err(std::io::Error::other)),
            HEARTBEAT_IDLE,
        );
        sse_stream_response(stream)
    } else {
        // Rewrite `model` to the gateway id for a consistent client view.
        let mut v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
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

pub(crate) async fn forward_responses(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut resp_body = if let Some(v) = variant {
        apply_variant(anthropic_to_responses(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_responses(body, &entry.model_id)
    };
    apply_entry_body(&mut resp_body, entry);
    let url = join_url(base, "responses");
    let req = with_session_headers(
        with_entry_headers(upstream_post(s, &url, bearer), entry),
        headers,
    );
    let resp = match send_json(req, &resp_body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if !stream {
        let v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
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
            for payload in drain_sse_payloads(&mut buf) {
                let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue; };
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
            yield Ok(empty_text_block_event().into_bytes());
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
            yield Ok::<_, std::io::Error>(terminal_error_event(&msg).into_bytes());
        }
    };
    sse_stream_response(out)
}

pub(crate) async fn forward_openai(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut oai_body = if let Some(v) = variant {
        apply_variant(anthropic_to_openai(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_openai(body, &entry.model_id)
    };
    apply_entry_body(&mut oai_body, entry);
    let url = join_url(base, "chat/completions");
    // Some OpenAI-compatible gateways also accept api-key header; harmless to send.
    let req = with_session_headers(
        with_entry_headers(upstream_post(s, &url, bearer), entry),
        headers,
    );
    let resp = match send_json(req, &oai_body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if !stream {
        let v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
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
            for payload in drain_sse_payloads(&mut buf) {
                let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue; };
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
            yield Ok(empty_text_block_event().into_bytes());
            tr.text_open = true;
        }
        if tr.tool_blocks.iter().any(|b| b.started) {
            stop_reason = "tool_use".to_string();
        }
        for line in tr.finish(&stop_reason, input_tokens, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = upstream_error {
            yield Ok::<_, std::io::Error>(terminal_error_event(&msg).into_bytes());
        }
    };
    sse_stream_response(out)
}
