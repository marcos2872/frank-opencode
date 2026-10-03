//! Shared request-part helpers (images, tool results, text blocks).

use serde_json::Value;

pub(crate) fn image_part_to_openai(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    let mt = src
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/jpeg");
    let data = src.get("data").and_then(Value::as_str)?;
    (!data.is_empty()).then(|| {
        serde_json::json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mt};base64,{data}")}
        })
    })
}

pub(crate) fn image_part_to_responses(b: &Value) -> Option<Value> {
    let src = b.get("source")?;
    let mt = src
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/jpeg");
    let data = src.get("data").and_then(Value::as_str)?;
    (!data.is_empty()).then(|| {
        serde_json::json!({
            "type": "input_image",
            "image_url": format!("data:{mt};base64,{data}")
        })
    })
}

/// Split a `tool_result` into its text and the images nested in its content.
///
/// Neither Chat Completions nor Responses has an error flag on a tool output,
/// so a failed result (`is_error`) is made explicit in the payload text
/// instead of being silently flattened to its (possibly empty) content.
pub(crate) fn tool_result_parts(b: &Value) -> (String, Vec<Value>, Vec<Value>) {
    let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let mut text = block_text(b).unwrap_or_default();
    if is_error {
        text = if text.is_empty() {
            "Error: tool execution failed".to_string()
        } else {
            format!("Error: {text}")
        };
    }
    let mut images_oai: Vec<Value> = vec![];
    let mut images_resp: Vec<Value> = vec![];
    if let Some(parts) = b.get("content").and_then(Value::as_array) {
        for p in parts {
            if let Some(img) = image_part_to_openai(p) {
                images_oai.push(img);
            }
            if let Some(img) = image_part_to_responses(p) {
                images_resp.push(img);
            }
        }
    }
    (text, images_oai, images_resp)
}

pub(crate) fn block_text(b: &Value) -> Option<String> {
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => b
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
        Some("tool_result") => {
            let c = b.get("content");
            match c {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Array(arr)) => Some(
                    arr.iter()
                        .filter_map(|x| {
                            x.get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Convert an Anthropic `/v1/messages` body into an OpenAI `/chat/completions` body.
