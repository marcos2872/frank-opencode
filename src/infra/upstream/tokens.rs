//! Local token estimation (no tokenizer; chars/4 + media costs).

use serde_json::Value;

pub fn estimate_tokens(body: &Value) -> u64 {
    let mut tokens: u64 = 0;
    fn add_text(tokens: &mut u64, s: &str) {
        *tokens += (s.chars().count() as u64 / 4).max(1);
    }
    if let Some(sys) = body.get("system") {
        match sys {
            Value::String(s) => add_text(&mut tokens, s),
            Value::Array(arr) => {
                for b in arr {
                    if let Some(t) = text_of(b) {
                        add_text(&mut tokens, t);
                    }
                }
            }
            _ => {}
        }
        tokens += 3; // system wrapper.
    }
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            tokens += 3; // per-message overhead (Anthropic/OpenAI style).
            match m.get("content") {
                Some(Value::String(s)) => add_text(&mut tokens, s),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match block_kind(b) {
                            BlockKind::Text => {
                                if let Some(t) = text_of(b) {
                                    add_text(&mut tokens, t);
                                }
                            }
                            BlockKind::Image | BlockKind::Document => {
                                tokens += estimate_media(b);
                            }
                            BlockKind::ToolUse => tokens += 8, // id + name + JSON.
                            BlockKind::ToolResult => {
                                if let Some(t) = text_of(b) {
                                    add_text(&mut tokens, t);
                                }
                                tokens += 4;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for t in tools {
            tokens += 4; // tool wrapper.
            if let Some(n) = t.get("name").and_then(|n| n.as_str()) {
                add_text(&mut tokens, n);
            }
            if let Some(d) = t.get("description").and_then(|d| d.as_str()) {
                add_text(&mut tokens, d);
            }
            if let Some(schema) = t.get("input_schema") {
                tokens += (schema.to_string().len() as u64 / 4).max(1);
            }
        }
    }
    tokens.max(1)
}

enum BlockKind {
    Text,
    Image,
    Document,
    ToolUse,
    ToolResult,
    Other,
}

fn block_kind(b: &Value) -> BlockKind {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => BlockKind::Text,
        Some("image") => BlockKind::Image,
        Some("document") => BlockKind::Document,
        Some("tool_use") => BlockKind::ToolUse,
        Some("tool_result") => BlockKind::ToolResult,
        _ => BlockKind::Other,
    }
}

fn text_of(b: &Value) -> Option<&str> {
    b.get("text").and_then(|t| t.as_str())
}

/// Tokens for an image/document block: fixed overhead + a share of the
/// base64 payload (base64 inflates content by ~33%; per the API's
/// documentation images have a large base cost regardless of size).
fn estimate_media(b: &Value) -> u64 {
    let is_image = matches!(block_kind(b), BlockKind::Image);
    let mut tokens: u64 = if is_image { 800 } else { 400 }; // base cost.
    if let Some(src) = b.get("source") {
        if let Some(data) = src.get("data").and_then(|d| d.as_str()) {
            tokens += (data.len() as u64 / 4) / 3; // decoded bytes → tokens.
        }
        if let Some(url) = src.get("url").and_then(|u| u.as_str()) {
            tokens += (url.len() as u64 / 4).max(1);
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::upstream::chat::anthropic_to_openai;
    use crate::infra::upstream::responses::anthropic_to_responses;

    #[test]
    fn max_tokens_floored_at_backend_minimum() {
        // Claude Code's model-switch probe sends `max_tokens: 1`; the zen
        // backend rejects output limits below 16 (muse-spark 400 on switch).
        let probe = serde_json::json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1
        });
        assert_eq!(anthropic_to_openai(&probe, "up")["max_tokens"], 16);
        assert_eq!(
            anthropic_to_responses(&probe, "up")["max_output_tokens"],
            16
        );

        // Values at or above the floor pass through unchanged on both paths.
        let normal = serde_json::json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 64000
        });
        assert_eq!(anthropic_to_openai(&normal, "up")["max_tokens"], 64000);
        assert_eq!(
            anthropic_to_responses(&normal, "up")["max_output_tokens"],
            64000
        );
    }
}