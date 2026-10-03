//! Common SSE pump for translated streams.
//!
//! Both translated protocols (Responses and Chat) share the same skeleton:
//! accumulate bytes, split on newlines, strip the `data:` prefix, skip
//! empties and `[DONE]`, parse JSON, extract usage, feed the translator,
//! then emit the well-formed tail (empty text block fallback, `finish`,
//! trailing error event). The per-protocol differences (usage field names
//! and stop/failure signals) are parameters, not copies.

use super::sse::{sse, sse_error};
use serde_json::Value;

/// Terminal state observed while pumping: usage counters, the stop reason
/// carried forward, and a trailing upstream failure.
#[derive(Debug, Default)]
pub struct PumpState {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub stop_reason: String,
    pub upstream_error: Option<String>,
}

impl PumpState {
    pub fn new(stop_reason: &str) -> Self {
        Self {
            stop_reason: stop_reason.to_string(),
            ..Default::default()
        }
    }
}

/// Extract `prompt/completion_tokens` (Chat) usage from an event.
pub fn chat_usage(v: &Value, st: &mut PumpState, set_translator_input: &mut dyn FnMut(u64)) {
    if let Some(u) = v.get("usage") {
        if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
            st.input_tokens = n;
            set_translator_input(n);
        }
        if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
            st.output_tokens = n;
        }
    }
}

/// Extract `input/output_tokens` (Responses, under `response.usage`).
pub fn responses_usage(v: &Value, st: &mut PumpState, set_translator_input: &mut dyn FnMut(u64)) {
    if let Some(r) = v.get("response") {
        if let Some(u) = r.get("usage") {
            if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                st.input_tokens = n;
                set_translator_input(n);
            }
            if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
                st.output_tokens = n;
            }
        }
    }
}

/// Split one raw SSE line into its JSON payload, or `None` when the line
/// carries nothing (`data:` empty or `[DONE]`).
pub fn sse_payload(line: &str) -> Option<Value> {
    let t = line.trim();
    let payload = t.strip_prefix("data:").map(|x| x.trim()).unwrap_or("");
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str::<Value>(payload).ok()
}

/// Empty text-block opener, emitted when the upstream never opened any
/// content so the Anthropic stream stays well-formed.
pub fn empty_text_block() -> String {
    sse(&serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))
}

/// Trailing `error` event for a mid-stream upstream failure.
pub fn stream_error(msg: &str) -> String {
    sse_error("api_error", msg)
}

/// Transport failure message shared by both pumps.
pub fn read_failure(source: &dyn std::fmt::Display) -> String {
    format!("upstream stream read failed: {source}")
}
