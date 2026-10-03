//! Protocol forwards over the shared request builder + SSE pump.
//!
//! Each forward does three things in order: build the upstream body
//! (translate → variant → catalog defaults), POST it via `request.rs`,
//! then answer non-streaming JSON or pump the translated SSE stream.
//! The wire details (headers, status mapping, framing) live in the
//! shared modules; only the per-protocol body/usage/stop mapping stays here.

use crate::api::errors::{gateway_error_response, response_failure_message};
use crate::api::headers::apply_entry_body;
use crate::api::request::{check_status, parse_json_body, post_upstream, sse_response, WireTarget};
use crate::api::state::{AppState, HEARTBEAT_IDLE};
use crate::domain::{CatalogEntry, GatewayError};
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, apply_variant_checked,
    openai_to_anthropic, responses_to_anthropic, sse_pump, with_heartbeat, ResponsesTranslator,
    StreamTranslator,
};
use axum::{
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use futures::StreamExt;
use serde_json::Value;

/// Parameters shared by all three forwards (replaces the 9-argument
/// signatures that needed `#[allow(clippy::too_many_arguments)]`).
pub(crate) struct ForwardCtx<'a> {
    pub state: &'a AppState,
    pub headers: &'a HeaderMap,
    pub body: &'a Value,
    pub entry: &'a CatalogEntry,
    pub base: &'a str,
    pub bearer: &'a secrecy::SecretString,
    pub gateway_model: &'a str,
    pub stream: bool,
    pub variant: Option<&'a str>,
}

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
    let ctx = ForwardCtx {
        state: s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    };
    forward_anthropic_ctx(&ctx, body).await
}

async fn forward_anthropic_ctx(ctx: &ForwardCtx<'_>, body: &mut Value) -> Response {
    body["model"] = Value::String(ctx.entry.model_id.clone());
    // Apply the selected variant to the raw body: the Messages API forwards
    // client fields verbatim, so `thinking`/`include` land unchanged. A
    // variant the Messages API cannot represent is a 400, not a silent no-op.
    if let Some(v) = ctx.variant {
        match apply_variant_checked(std::mem::take(body), ctx.entry, v) {
            Ok(applied) => *body = applied,
            Err(e) => {
                return gateway_error_response(&GatewayError::VariantNotRepresentable {
                    detail: e.to_string(),
                })
            }
        }
    }
    apply_entry_body(body, ctx.entry);
    // `stream` passthrough stays as the client sent it.
    let resp = match post_upstream(
        ctx.state,
        ctx.headers,
        ctx.entry,
        ctx.base,
        ctx.bearer,
        WireTarget::Anthropic,
        body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return e,
    };
    let resp = match check_status(resp, ctx.entry, ctx.gateway_model, ctx.base, body).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if ctx.stream {
        // Byte passthrough, but inject `ping` during upstream silence so the
        // client's stream watchdog doesn't abort long thinking pauses.
        let stream = with_heartbeat(
            resp.bytes_stream()
                .map(|c| c.map(|b| b.to_vec()).map_err(std::io::Error::other)),
            HEARTBEAT_IDLE,
        );
        sse_response(stream)
    } else {
        // Rewrite `model` to the gateway id for a consistent client view.
        let mut v: Value = match parse_json_body(resp).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        if v.get("model").is_some() {
            v["model"] = Value::String(ctx.gateway_model.to_string());
        }
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = ctx.gateway_model, "upstream ok");
        }
        Json(v).into_response()
    }
}

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
    let ctx = ForwardCtx {
        state: s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    };
    forward_responses_ctx(&ctx).await
}

async fn forward_responses_ctx(ctx: &ForwardCtx<'_>) -> Response {
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut resp_body = if let Some(v) = ctx.variant {
        apply_variant(
            anthropic_to_responses(ctx.body, &ctx.entry.model_id),
            ctx.entry,
            v,
        )
    } else {
        anthropic_to_responses(ctx.body, &ctx.entry.model_id)
    };
    apply_entry_body(&mut resp_body, ctx.entry);
    let resp = match post_upstream(
        ctx.state,
        ctx.headers,
        ctx.entry,
        ctx.base,
        ctx.bearer,
        WireTarget::Responses,
        &resp_body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return e,
    };
    let resp = match check_status(resp, ctx.entry, ctx.gateway_model, ctx.base, ctx.body).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if !ctx.stream {
        let v: Value = match parse_json_body(resp).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = ctx.gateway_model, "upstream ok");
        }
        return Json(responses_to_anthropic(&v, ctx.gateway_model)).into_response();
    }
    // Translated streaming: Responses SSE -> Anthropic SSE.
    let gw = ctx.gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = ResponsesTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        let mut pinned = Box::pin(byte_stream);
        let mut st = sse_pump::PumpState::new("end_turn");
        let mut incomplete = false;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    st.upstream_error = Some(sse_pump::read_failure(&e));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let Some(v) = sse_pump::sse_payload(&text) else { continue; };
                // Usage + status arrive on response.completed.
                sse_pump::responses_usage(&v, &mut st, &mut |n| tr.input_tokens = n);
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("response.incomplete") => incomplete = true,
                    // The upstream failed after the stream opened: surface it
                    // rather than ending 200 with no explanation.
                    Some("response.failed") => {
                        st.upstream_error = Some(response_failure_message(&v));
                    }
                    _ => {}
                }
                for ev in tr.feed(&v) {
                    yield Ok(ev.into_bytes());
                }
            }
        }
        if !tr.text_open && !tr.has_tools() {
            yield Ok(sse_pump::empty_text_block().into_bytes());
            tr.text_open = true;
        }
        let reason = if tr.has_tools() {
            "tool_use"
        } else if incomplete {
            "max_tokens"
        } else {
            "end_turn"
        };
        for line in tr.finish(reason, st.input_tokens, st.output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = st.upstream_error {
            yield Ok::<_, std::io::Error>(sse_pump::stream_error(&msg).into_bytes());
        }
    };
    sse_response(with_heartbeat(out, HEARTBEAT_IDLE))
}

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
    let ctx = ForwardCtx {
        state: s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    };
    forward_openai_ctx(&ctx).await
}

async fn forward_openai_ctx(ctx: &ForwardCtx<'_>) -> Response {
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut oai_body = if let Some(v) = ctx.variant {
        apply_variant(
            anthropic_to_openai(ctx.body, &ctx.entry.model_id),
            ctx.entry,
            v,
        )
    } else {
        anthropic_to_openai(ctx.body, &ctx.entry.model_id)
    };
    apply_entry_body(&mut oai_body, ctx.entry);
    let resp = match post_upstream(
        ctx.state,
        ctx.headers,
        ctx.entry,
        ctx.base,
        ctx.bearer,
        WireTarget::Chat,
        &oai_body,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return e,
    };
    let resp = match check_status(resp, ctx.entry, ctx.gateway_model, ctx.base, ctx.body).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    if !ctx.stream {
        let v: Value = match parse_json_body(resp).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        if let Some(uid) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_str())
        {
            tracing::debug!(upstream_id = uid, model = ctx.gateway_model, "upstream ok");
        }
        return Json(openai_to_anthropic(&v, ctx.gateway_model)).into_response();
    }
    // Translated streaming: OpenAI SSE -> Anthropic SSE.
    let gw = ctx.gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = StreamTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        let mut pinned = Box::pin(byte_stream);
        let mut st = sse_pump::PumpState::new("end_turn");
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    st.upstream_error = Some(sse_pump::read_failure(&e));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            // Process complete lines.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line);
                let Some(v) = sse_pump::sse_payload(&text) else { continue; };
                sse_pump::chat_usage(&v, &mut st, &mut |n| tr.input_tokens = n);
                if let Some(fr) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|c| c.get("finish_reason")).and_then(|f| f.as_str()) {
                    st.stop_reason = match fr {
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
            yield Ok(sse_pump::empty_text_block().into_bytes());
            tr.text_open = true;
        }
        if tr.tool_blocks.iter().any(|b| b.started) {
            st.stop_reason = "tool_use".to_string();
        }
        for line in tr.finish(&st.stop_reason, st.input_tokens, st.output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = st.upstream_error {
            yield Ok::<_, std::io::Error>(sse_pump::stream_error(&msg).into_bytes());
        }
    };
    sse_response(with_heartbeat(out, HEARTBEAT_IDLE))
}
