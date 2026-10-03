//! Gateway error rendering and privacy-safe diagnostics.

use crate::domain::{CatalogEntry, GatewayError};
use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::Value;

pub(crate) fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": err_type, "message": msg}});
    (status, Json(body)).into_response()
}

/// Single mapping from a typed error to the Anthropic wire shape.
/// `upstream_error_response` keeps its own path: it normalizes an arbitrary
/// upstream body instead of rendering a typed error.
pub(crate) fn gateway_error_response(e: &GatewayError) -> Response {
    let status = StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::BAD_GATEWAY);
    anthropic_error(status, e.error_type(), &e.to_string())
}

pub(crate) fn upstream_error_response(status: StatusCode, text: &str) -> Response {
    let value = serde_json::from_str::<Value>(text).ok();
    let message = value
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| body_preview(text));
    let error_type = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "invalid_request_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ => "api_error",
    };
    anthropic_error(status, error_type, &message)
}

pub(crate) fn response_failure_message(v: &Value) -> String {
    let r = v.get("response").unwrap_or(v);
    let picked = r
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| r.get("error").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| {
            r.get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match picked {
        Some(s) if !s.is_empty() => format!("upstream response failed: {s}"),
        _ => "upstream response failed".to_string(),
    }
}

fn body_preview(s: &str) -> String {
    const MAX: usize = 500;
    let t = s.trim();
    if t.len() <= MAX {
        return t.to_string();
    }
    let end = t
        .char_indices()
        .take_while(|(index, _)| *index < MAX)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0);
    let mut out = t[..end].to_string();
    out.push('…');
    out
}

pub(crate) fn log_upstream_error(
    entry: &CatalogEntry,
    gateway_model: &str,
    base: &str,
    status: StatusCode,
    text: &str,
    req_summary: &serde_json::Value,
) {
    tracing::warn!(
        gateway_model = %gateway_model,
        opencode_ref = %entry.qualified(),
        entry_id = %entry.id,
        provider = %entry.provider_id,
        base_url = %base,
        status = status.as_u16(),
        body = %body_preview(text),
        req = %req_summary,
        "upstream rejected request"
    );
}

/// Privacy-safe shape of the Anthropic request that failed: counts, block
/// kinds and sizes only, never prompt/tool content. Lets us tell "poisoned
/// history in this session" (e.g. a tool_result shape the translator or the
/// upstream rejects) apart from "model down" without logging user data.
pub(crate) fn request_summary(body: &Value) -> Value {
    let mut msgs = vec![];
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr.iter().take(50) {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            match m.get("content") {
                Some(Value::String(s)) => msgs.push(serde_json::json!({
                    "role": role, "kind": "text", "chars": s.chars().count()
                })),
                Some(Value::Array(blocks)) => {
                    let kinds: Vec<String> = blocks
                        .iter()
                        .map(|b| {
                            let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                            // Text length without content: tool I/O still needs a size hint.
                            let chars = b
                                .get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.chars().count())
                                .unwrap_or(0);
                            if chars > 0 {
                                format!("{t}:{chars}ch")
                            } else {
                                t.to_string()
                            }
                        })
                        .collect();
                    msgs.push(serde_json::json!({"role": role, "kind": kinds}));
                }
                _ => msgs.push(serde_json::json!({"role": role, "kind": "other"})),
            }
        }
    }
    let tools: Vec<String> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .take(30)
                .map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "messages": msgs.len(),
        "detail": msgs,
        "tools": tools,
        "tool_choice": body.get("tool_choice").and_then(|t| t.get("type").and_then(|x| x.as_str())),
        "max_tokens": body.get("max_tokens"),
        "stream": body.get("stream"),
        "system": body.get("system").map(|s| if s.is_string() { "text" } else { "blocks" }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_summary_counts_without_content() {
        let body = json!({
            "model": "claude-x",
            "max_tokens": 128,
            "stream": true,
            "system": "secret-system",
            "messages": [
                {"role": "user", "content": "secret-prompt"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "secret-reply"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "secret-path"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "secret-file-contents"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}},
                ]},
            ],
            "tools": [{"name": "Read", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
        });
        let s = request_summary(&body);
        let rendered = s.to_string();
        for secret in [
            "secret-prompt",
            "secret-reply",
            "secret-path",
            "secret-file-contents",
            "secret-system",
            "AAA",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        assert_eq!(s["messages"], 3);
        assert_eq!(s["tools"], json!(["Read"]));
        assert_eq!(s["max_tokens"], 128);
    }

    #[test]
    fn body_preview_truncates_at_utf8_boundary() {
        let long = format!("{}x", "é".repeat(300));
        let preview = body_preview(&long);
        assert!(preview.ends_with('…'));
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }

    #[test]
    fn body_preview_truncates() {
        assert_eq!(body_preview("  ok  "), "ok");
        let long = "x".repeat(600);
        let p = body_preview(&long);
        assert!(p.len() < 600 && p.ends_with('…'), "{p}");
    }

    // -- mock_classifier ---------------------------------------------------
}