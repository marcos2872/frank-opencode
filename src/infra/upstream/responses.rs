//! Anthropic <-> OpenAI Responses API translation.

use super::shared::{
    block_text, floor_output_tokens, image_part_to_responses, parse_args, tool_result_parts,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Responses API (`/responses`: Go GPT/Grok/Muse rows)
// ---------------------------------------------------------------------------

/// Convert an Anthropic `/v1/messages` body into an OpenAI `/responses` body.
pub fn anthropic_to_responses(body: &Value, upstream_model: &str) -> Value {
    // `tool_use` ids dropped for having no name (see `anthropic_to_openai`): the
    // matching `tool_result` must be dropped too, or Responses sees an orphan
    // `function_call_output`.
    let mut dropped_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut input: Vec<Value> = vec![];
    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match m.get("content") {
                Some(Value::String(s)) => {
                    let kind = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    input.push(serde_json::json!({
                        "role": role,
                        "content": [{"type": kind, "text": s}]
                    }));
                }
                Some(Value::Array(blocks)) => {
                    let mut texts: Vec<String> = vec![];
                    let mut images: Vec<Value> = vec![];
                    let mut calls: Vec<Value> = vec![];
                    let mut outputs: Vec<Value> = vec![];
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    texts.push(t.to_string());
                                }
                            }
                            Some("image") => {
                                if let Some(img) = image_part_to_responses(b) {
                                    images.push(img);
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                if name.is_empty() {
                                    if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                                        if !id.is_empty() {
                                            dropped_tool_ids.insert(id.to_string());
                                        }
                                    }
                                } else {
                                    calls.push(serde_json::json!({
                                        "type": "function_call",
                                        "call_id": b.get("id").cloned().unwrap_or(Value::String("call_0".into())),
                                        "name": name,
                                        "arguments": serde_json::to_string(b.get("input").unwrap_or(&Value::Object(Default::default()))).unwrap_or_else(|_| "{}".into())
                                    }));
                                }
                            }
                            Some("tool_result") => {
                                let id = b
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if dropped_tool_ids.contains(&id) {
                                    continue;
                                }
                                let (txt, _, nested_images) = tool_result_parts(b);
                                outputs.push(serde_json::json!({
                                    "type": "function_call_output",
                                    "call_id": id,
                                    "output": txt
                                }));
                                // Responses has no error flag and no place for
                                // images inside a function_call_output, so they
                                // follow as a user input_image message. The
                                // ordered input list keeps them in place.
                                if !nested_images.is_empty() {
                                    input.push(serde_json::json!({
                                        "role": "user",
                                        "content": nested_images
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                    if role == "assistant" {
                        if !texts.join("\n").trim().is_empty() {
                            input.push(serde_json::json!({
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": texts.join("\n")}]
                            }));
                        }
                        input.extend(calls);
                        input.extend(outputs);
                    } else {
                        if !texts.join("\n").trim().is_empty() || !images.is_empty() {
                            let mut content: Vec<Value> = vec![];
                            let t = texts.join("\n");
                            if !t.is_empty() {
                                content.push(serde_json::json!({"type": "input_text", "text": t}));
                            }
                            content.extend(images);
                            input.push(serde_json::json!({"role": "user", "content": content}));
                        }
                        input.extend(outputs);
                    }
                }
                _ => {}
            }
        }
    }

    let mut out = serde_json::json!({
        "model": upstream_model,
        "input": input,
    });
    if let Some(sys) = body.get("system") {
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(block_text)
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            out["instructions"] = Value::String(text);
        }
    }
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.get("name").cloned().unwrap_or(Value::String("".into())),
                    "description": t.get("description").cloned().unwrap_or(Value::String("".into())),
                    "parameters": t.get("input_schema").cloned().unwrap_or(serde_json::json!({"type":"object"}))
                })
            })
            .collect();
        out["tools"] = Value::Array(mapped);
        if let Some(tc) = body.get("tool_choice") {
            // Responses API accepts "auto" | "required" | "none" | {"type":"function",...}.
            out["tool_choice"] = match tc.get("type").and_then(|t| t.as_str()) {
                Some("any") => serde_json::json!("required"),
                Some("none") => serde_json::json!("none"),
                Some("tool") => {
                    if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
                        serde_json::json!({"type":"function","name":name})
                    } else {
                        serde_json::json!("required")
                    }
                }
                _ => serde_json::json!("auto"),
            };
        }
    }
    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(m) = body.get("max_tokens") {
        out["max_output_tokens"] = floor_output_tokens(m);
    }
    if body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
    {
        out["stream"] = Value::Bool(true);
    }
    out
}
/// Convert an OpenAI Responses API response into an Anthropic Messages response.
pub fn responses_to_anthropic(resp: &Value, gateway_model: &str) -> Value {
    let msg_id = format!(
        "msg_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    );
    let items = resp
        .get("output")
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let mut content: Vec<Value> = vec![];
    for item in &items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            if !t.is_empty() {
                                content.push(serde_json::json!({"type": "text", "text": t}));
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                content.push(serde_json::json!({
                    "type": "tool_use",
                    "id": item.get("call_id").cloned().unwrap_or(Value::String("toolu_0".into())),
                    "name": item.get("name").cloned().unwrap_or(Value::String("".into())),
                    "input": parse_args(item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}"))
                }));
            }
            Some("reasoning") => {}
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }
    let has_tools = content
        .iter()
        .any(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    let status = resp
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("completed");
    let incomplete_max = resp
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|r| r.as_str())
        == Some("max_output_tokens");
    let stop_reason = if has_tools {
        "tool_use"
    } else if status == "incomplete" || incomplete_max {
        "max_tokens"
    } else {
        "end_turn"
    };
    let usage = resp.get("usage");
    serde_json::json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": gateway_model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": usage.and_then(|u| u.get("input_tokens")).and_then(|x| x.as_u64()).unwrap_or(0),
            "output_tokens": usage.and_then(|u| u.get("output_tokens")).and_then(|x| x.as_u64()).unwrap_or(0)
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[test]
    fn converts_to_responses_request() {
        let body = serde_json::json!({
            "model": "x",
            "system": "be concise",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "looking"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ],
            "tools": [{"name": "Read", "description": "d", "input_schema": {"type": "object"}}],
            "max_tokens": 4096
        });
        let r = anthropic_to_responses(&body, "upstream");
        assert_eq!(r["model"], "upstream");
        assert_eq!(r["instructions"], "be concise");
        assert_eq!(r["max_output_tokens"], 4096);
        let input = r["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "function_call"));
        assert!(input.iter().any(|i| i["type"] == "function_call_output"));
        assert_eq!(r["tools"][0]["name"], "Read");
    }
    #[test]
    fn converts_responses_response_with_tools() {
        let resp = serde_json::json!({
            "status": "completed",
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "x"}]},
                {"type": "function_call", "call_id": "c1", "name": "Bash", "arguments": "{\"cmd\":\"ls\"}"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 7}
        });
        let a = responses_to_anthropic(&resp, "gw");
        assert_eq!(a["stop_reason"], "tool_use");
        assert_eq!(a["content"][1]["name"], "Bash");
        assert_eq!(a["usage"]["output_tokens"], 7);
    }
    #[test]
    fn responses_reasoning_item_is_omitted_not_fabricated() {
        // Responses `reasoning` items carry no Anthropic-valid form (no real
        // signature or redacted content), so they are dropped rather than
        // emitted as a fake `thinking` block.
        let resp = serde_json::json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "why"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "answer"}]}
            ],
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let a = responses_to_anthropic(&resp, "gw");
        let content = a["content"].as_array().unwrap();
        assert!(!content.iter().any(|c| c["type"] == "thinking"));
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "answer");
    }
}
