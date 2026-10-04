//! Anthropic <-> OpenAI Chat Completions translation.

use super::shared::{block_text, floor_output_tokens, parse_args, tool_result_parts};
use serde_json::Value;

/// Convert an Anthropic `/v1/messages` body into an OpenAI `/chat/completions` body.
pub fn anthropic_to_openai(body: &Value, upstream_model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    // `tool_use` ids dropped for having no name: their `tool_result` must be
    // dropped too, or the upstream sees an orphan `tool` message referencing a
    // call that was never sent.
    let mut dropped_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // system: string | array of text blocks -> single system message.
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
            messages.push(serde_json::json!({"role": "system", "content": text}));
        }
    }

    if let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = m.get("content");
            match content {
                Some(Value::String(s)) => {
                    messages.push(serde_json::json!({"role": role, "content": s}));
                }
                Some(Value::Array(blocks)) => {
                    let mut texts: Vec<String> = vec![];
                    let mut tool_calls: Vec<Value> = vec![];
                    let mut images: Vec<Value> = vec![];
                    let mut tool_results: Vec<(String, String, Vec<Value>)> = vec![];
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    texts.push(t.to_string());
                                }
                            }
                            Some("image") => {
                                if let Some(src) = b.get("source") {
                                    let mt = src
                                        .get("media_type")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("image/jpeg");
                                    let data =
                                        src.get("data").and_then(|x| x.as_str()).unwrap_or("");
                                    if !data.is_empty() {
                                        images.push(serde_json::json!({
                                            "type": "image_url",
                                            "image_url": {"url": format!("data:{mt};base64,{data}")}
                                        }));
                                    }
                                }
                            }
                            Some("tool_use") => {
                                // A `tool_use` without a name has no valid OpenAI
                                // form: strict upstreams reject it with
                                // `tool_calls[].function.name` missing. Drop it
                                // (and its `tool_result` below) instead of
                                // poisoning every later turn of the session.
                                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                if name.is_empty() {
                                    if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                                        if !id.is_empty() {
                                            dropped_tool_ids.insert(id.to_string());
                                        }
                                    }
                                } else {
                                    tool_calls.push(serde_json::json!({
                                        "id": b.get("id").cloned().unwrap_or(Value::String("call_0".into())),
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": serde_json::to_string(b.get("input").unwrap_or(&Value::Object(Default::default()))).unwrap_or_else(|_| "{}".into())
                                        }
                                    }));
                                }
                            }
                            Some("tool_result") => {
                                let id = b
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                // Skip the result of a `tool_use` we dropped for
                                // missing a name (see above); otherwise the `tool`
                                // message would reference a call never sent.
                                if dropped_tool_ids.contains(&id) {
                                    continue;
                                }
                                let (txt, nested_images, _) = tool_result_parts(b);
                                tool_results.push((id, txt, nested_images));
                            }
                            _ => {}
                        }
                    }
                    if !tool_results.is_empty() {
                        // Each tool_result becomes its own `tool` message.
                        // They must immediately follow the assistant
                        // `tool_calls` message: strict OpenAI-compatible
                        // upstreams 400 otherwise ("must be followed by tool
                        // messages"). Any accompanying text goes AFTER as its
                        // own user message.
                        let mut trailing_content: Vec<Value> = vec![];
                        for (id, txt, nested_images) in tool_results {
                            messages.push(serde_json::json!({
                                "role": "tool", "tool_call_id": id, "content": txt
                            }));
                            if !nested_images.is_empty() {
                                if !txt.is_empty() {
                                    trailing_content
                                        .push(serde_json::json!({"type":"text","text":txt}));
                                }
                                trailing_content.extend(nested_images);
                            }
                        }
                        let t = texts.join("\n");
                        if !t.trim().is_empty() {
                            trailing_content.push(serde_json::json!({"type":"text","text":t}));
                        }
                        if !trailing_content.is_empty() {
                            // Text-only trailing content stays a plain
                            // string: strict OpenAI-compatible upstreams are
                            // less tolerant of a content-part array. Images
                            // nested in a tool_result force the array shape.
                            let content = if trailing_content.iter().all(|p| p["type"] == "text") {
                                Value::String(
                                    trailing_content
                                        .iter()
                                        .filter_map(|p| p["text"].as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                )
                            } else {
                                Value::Array(trailing_content)
                            };
                            messages.push(serde_json::json!({"role":"user","content":content}));
                        }
                    } else if !tool_calls.is_empty() {
                        let mut msg = serde_json::json!({
                            "role": "assistant",
                            "content": if texts.is_empty() { Value::Null } else { Value::String(texts.join("\n")) },
                            "tool_calls": tool_calls
                        });
                        if texts.is_empty() {
                            msg.as_object_mut().unwrap().remove("content");
                        }
                        messages.push(msg);
                    } else if !images.is_empty() {
                        let mut content: Vec<Value> = vec![];
                        let t = texts.join("\n");
                        if !t.is_empty() {
                            content.push(serde_json::json!({"type": "text", "text": t}));
                        }
                        content.extend(images);
                        messages.push(serde_json::json!({"role": role, "content": content}));
                    } else {
                        messages
                            .push(serde_json::json!({"role": role, "content": texts.join("\n")}));
                    }
                }
                _ => {
                    messages.push(serde_json::json!({"role": role, "content": ""}));
                }
            }
        }
    }

    let mut out = serde_json::json!({
        "model": upstream_model,
        "messages": messages,
    });

    // tools
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(Value::String("".into())),
                        "description": t.get("description").cloned().unwrap_or(Value::String("".into())),
                        "parameters": t.get("input_schema").cloned().unwrap_or(serde_json::json!({"type":"object"}))
                    }
                })
            })
            .collect();
        out["tools"] = Value::Array(mapped);
        if let Some(tc) = body.get("tool_choice") {
            // Anthropic {"type":"auto"|"any"|"tool"|"none",...} -> OpenAI Chat.
            let choice = match tc.get("type").and_then(|t| t.as_str()) {
                Some("any") => serde_json::json!("required"),
                Some("none") => serde_json::json!("none"),
                Some("tool") => {
                    if let Some(name) = tc.get("name").and_then(|n| n.as_str()) {
                        serde_json::json!({"type":"function","function":{"name":name}})
                    } else {
                        serde_json::json!("required")
                    }
                }
                _ => serde_json::json!("auto"),
            };
            out["tool_choice"] = choice;
        }
    }

    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(m) = body.get("max_tokens") {
        out["max_tokens"] = floor_output_tokens(m);
    }
    if let Some(stop) = body.get("stop_sequences") {
        out["stop"] = stop.clone();
    }
    if body
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
    {
        out["stream"] = Value::Bool(true);
        out["stream_options"] = serde_json::json!({"include_usage": true});
    }
    out
}
// ---------------------------------------------------------------------------
// OpenAI -> Anthropic (non-streaming)
// ---------------------------------------------------------------------------

/// Convert an OpenAI Chat Completion response into an Anthropic Messages response.
pub fn openai_to_anthropic(resp: &Value, gateway_model: &str) -> Value {
    let msg_id = format!(
        "msg_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    );
    let choice = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);
    let message = choice.get("message").cloned().unwrap_or(Value::Null);
    let finish = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");

    let mut content: Vec<Value> = vec![];
    match message.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            content.push(serde_json::json!({"type": "text", "text": s}));
        }
        Some(Value::Array(arr)) => {
            for p in arr {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    content.push(serde_json::json!({"type": "text", "text": t}));
                }
            }
        }
        _ => {}
    }
    let tool_calls = message
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    for tc in &tool_calls {
        let id = tc
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("toolu_0")
            .to_string();
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let args = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap_or("{}");
        content.push(serde_json::json!({
            "type": "tool_use", "id": id, "name": name, "input": parse_args(args)
        }));
    }
    if content.is_empty() {
        content.push(serde_json::json!({"type": "text", "text": ""}));
    }

    let stop_reason = if !tool_calls.is_empty() {
        "tool_use"
    } else {
        match finish {
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            _ => "end_turn",
        }
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
            "input_tokens": usage.and_then(|u| u.get("prompt_tokens")).and_then(|x| x.as_u64()).unwrap_or(0),
            "output_tokens": usage.and_then(|u| u.get("completion_tokens")).and_then(|x| x.as_u64()).unwrap_or(0)
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[test]
    fn converts_system_and_tool_use() {
        let body = serde_json::json!({
            "model": "x",
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
            "max_tokens": 10
        });
        let oai = anthropic_to_openai(&body, "upstream");
        assert_eq!(oai["model"], "upstream");
        let msgs = oai["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        // assistant tool call preserved
        let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(asst["tool_calls"][0]["id"], "t1");
        // tool result becomes tool role
        assert!(msgs.iter().any(|m| m["role"] == "tool"));
        assert_eq!(oai["tools"][0]["function"]["name"], "Read");
    }
    #[test]
    fn mixed_tool_result_and_text_keeps_tool_adjacency() {
        // Strict OpenAI-compatible upstreams 400 when a `user` message sits
        // between assistant `tool_calls` and their `tool` responses, so the
        // `tool` messages must come first and the text after.
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                {"role": "user", "content": "read it"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "looking"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "file contents"},
                    {"type": "text", "text": "now summarize"}
                ]}
            ],
            "max_tokens": 10
        });
        let oai = anthropic_to_openai(&body, "upstream");
        let msgs = oai["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
        assert_eq!(msgs[2]["tool_call_id"], "t1");
        assert_eq!(msgs[3]["content"], "now summarize");
    }
    #[test]
    fn converts_openai_response_with_tools() {
        let resp = serde_json::json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": "x",
                "tool_calls": [{"id": "c1", "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls\"}"}}]
            }}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7}
        });
        let a = openai_to_anthropic(&resp, "gw");
        assert_eq!(a["stop_reason"], "tool_use");
        assert_eq!(a["content"][1]["name"], "Bash");
        assert_eq!(a["usage"]["input_tokens"], 5);
    }
}
