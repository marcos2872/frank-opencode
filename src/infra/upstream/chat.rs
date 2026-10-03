//! Chat Completions translation (Anthropic <-> OpenAI `/chat/completions`).

use super::common::{floor_output_tokens, new_message_id};
use super::shared::{block_text, image_part_to_openai, tool_result_parts};
use serde_json::Value;

pub fn anthropic_to_openai(body: &Value, upstream_model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();

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
                                tool_calls.push(serde_json::json!({
                                    "id": b.get("id").cloned().unwrap_or(Value::String("call_0".into())),
                                    "type": "function",
                                    "function": {
                                        "name": b.get("name").cloned().unwrap_or(Value::String("".into())),
                                        "arguments": serde_json::to_string(b.get("input").unwrap_or(&Value::Object(Default::default()))).unwrap_or_else(|_| "{}".into())
                                    }
                                }));
                            }
                            Some("tool_result") => {
                                let id = b
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
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
// Model variants
// ---------------------------------------------------------------------------

fn parse_args(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(s).unwrap_or(Value::Object(Default::default()))
}

/// Convert an OpenAI Chat Completion response into an Anthropic Messages response.

pub fn openai_to_anthropic(resp: &Value, gateway_model: &str) -> Value {
    let msg_id = new_message_id();
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

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Responses API (`/responses`: Go GPT/Grok/Muse rows)
// ---------------------------------------------------------------------------

/// Convert an Anthropic `/v1/messages` body into an OpenAI `/responses` body.

pub struct StreamTranslator {
    pub gateway_model: String,
    pub msg_id: String,
    pub text_open: bool,
    pub text_closed: bool,
    pub tool_blocks: Vec<ToolBlock>,
    pub message_started: bool,
    pub input_tokens: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ToolBlock {
    pub index: usize, // anthropic block index (1-based after text block 0)
    pub id: String,
    pub name: String,
    pub started: bool,
    pub closed: bool,
}

impl StreamTranslator {
    pub fn new(gateway_model: &str) -> Self {
        Self {
            gateway_model: gateway_model.to_string(),
            msg_id: new_message_id(),
            ..Default::default()
        }
    }

    pub fn prefix(&mut self) -> Vec<String> {
        if self.message_started {
            return vec![];
        }
        self.message_started = true;
        vec![sse(&serde_json::json!({
            "type": "message_start",
            "message": {"id": self.msg_id, "type": "message", "role": "assistant",
                "model": self.gateway_model, "content": [],
                "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}}
        }))]
    }

    fn ensure_text(&mut self, out: &mut Vec<String>) {
        if !self.text_open {
            self.text_open = true;
            out.push(sse(&serde_json::json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}
            })));
        }
    }

    /// Feed one parsed OpenAI chunk (`data:` JSON), return Anthropic `data:` lines.
    pub fn feed(&mut self, chunk: &Value) -> Vec<String> {
        let mut out = vec![];
        let choice = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first());
        let Some(choice) = choice else { return out };
        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

        // text
        if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
            if !t.is_empty() {
                self.ensure_text(&mut out);
                out.push(sse(&serde_json::json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": t}
                })));
            }
        }
        // tool calls
        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for tc in calls {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let has_identity = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                    || tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty())
                    || tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                        .is_some_and(|s| !s.is_empty());
                if has_identity && !self.text_open {
                    self.ensure_text(&mut out);
                }
                while self.tool_blocks.len() <= idx {
                    let n = self.tool_blocks.len();
                    self.tool_blocks.push(ToolBlock {
                        index: 1 + n,
                        ..Default::default()
                    });
                }
                let block = &mut self.tool_blocks[idx];
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        block.id = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(|x| x.as_str()) {
                        if !name.is_empty() {
                            block.name = name.to_string();
                        }
                    }
                }
                if !block.started && has_identity {
                    block.started = true;
                    if block.id.is_empty() {
                        block.id = format!("toolu_{idx}");
                    }
                    let bi = block.index;
                    let id = block.id.clone();
                    let name = block.name.clone();
                    out.push(sse(&serde_json::json!({
                        "type": "content_block_start", "index": bi,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    })));
                }
                if let Some(args) = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                {
                    if !args.is_empty() && block.started {
                        let bi = block.index;
                        out.push(sse(&serde_json::json!({
                            "type": "content_block_delta", "index": bi,
                            "delta": {"type": "input_json_delta", "partial_json": args}
                        })));
                    }
                }
            }
        }
        out
    }

    pub fn finish(
        &mut self,
        stop_reason: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Vec<String> {
        let mut out = vec![];
        if self.text_open && !self.text_closed {
            self.text_closed = true;
            out.push(sse(
                &serde_json::json!({"type": "content_block_stop", "index": 0}),
            ));
        }
        for b in &self.tool_blocks {
            if b.started && !b.closed {
                out.push(sse(
                    &serde_json::json!({"type": "content_block_stop", "index": b.index}),
                ));
            }
        }
        out.push(sse(&serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
        })));
        out.push(sse(&serde_json::json!({"type": "message_stop"})));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CatalogEntry, CatalogSettings};
    use crate::infra::upstream::responses::anthropic_to_responses;

    fn entry_with_variants(pkg: &str, variants: Vec<(String, Option<String>)>) -> CatalogEntry {
        CatalogEntry {
            id: "m".to_string(),
            model_id: "m".to_string(),
            provider_id: "opencode".to_string(),
            name: "m".to_string(),
            package: pkg.to_string(),
            settings: CatalogSettings::default(),
            limit: None,
            enabled: true,
            headers: None,
            body: None,
            variants: variants
                .into_iter()
                .map(|(id, effort)| crate::domain::ModelVariant {
                    id,
                    settings: crate::domain::ModelVariantSettings {
                        reasoning_effort: effort,
                        ..Default::default()
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn tool_result_error_and_images_survive_translation() {
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                {"role": "user", "content": "look"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
                     "content": [
                        {"type": "text", "text": "boom"},
                        {"type": "image", "source": {"type":"base64","media_type":"image/png","data":"QUJD"}}
                     ]}
                ]}
            ],
            "max_tokens": 10
        });

        // Chat Completions: explicit error text + nested image in a follow-up.
        let oai = anthropic_to_openai(&body, "up");
        let msgs = oai["messages"].as_array().unwrap();
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"], "Error: boom");
        let img_holder = msgs
            .iter()
            .find(|m| m["role"] == "user" && m["content"].is_array())
            .expect("trailing user message with nested image");
        let parts = img_holder["content"].as_array().unwrap();
        assert!(parts.iter().any(|p| p["type"] == "image_url"));

        // Responses: function_call_output text is prefixed, image follows as
        // an input_image item.
        let resp = anthropic_to_responses(&body, "up");
        let input = resp["input"].as_array().unwrap();
        let out = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(out["output"], "Error: boom");
        assert!(input
            .iter()
            .any(|i| i["role"] == "user" && i["content"][0]["type"] == "input_image"));
    }

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
    fn tool_choice_none_maps_to_none() {
        for f in [anthropic_to_openai, anthropic_to_responses] {
            let body = serde_json::json!({
                "model": "x",
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "Read", "description": "d", "input_schema": {"type": "object"}}],
                "tool_choice": {"type": "none"},
                "max_tokens": 5
            });
            assert_eq!(f(&body, "up")["tool_choice"], "none");
        }
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

    #[test]
    fn stream_translator_text_and_tool() {
        let mut t = StreamTranslator::new("gw");
        assert_eq!(t.prefix().len(), 1);
        let c1 = serde_json::json!({"choices": [{"delta": {"content": "hi"}}]});
        let e1 = t.feed(&c1);
        assert!(e1.iter().any(|e| e.contains("text_delta")));
        let c2 = serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"p\":"}}]}}]});
        let e2 = t.feed(&c2);
        assert!(e2.iter().any(|e| e.contains("tool_use")));
        let end = t.finish("tool_use", 0, 3);
        assert!(end.iter().any(|e| e.contains("message_stop")));
    }

    #[test]
    fn stream_translator_opens_tool_when_args_arrive_first() {
        // Strict gateways may stream arguments before the id/name. The block
        // must still open (with a generated id/name), and the text block keeps
        // index 0 so tool indices stay monotonic.
        let mut t = StreamTranslator::new("gw");
        let _ = t.prefix();
        let ev = t.feed(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"p\":"}}
            ]}}]
        }));
        let started = ev
            .iter()
            .find(|e| e.contains("content_block_start") && e.contains("tool_use"))
            .expect("tool block opened from arguments alone");
        assert!(started.contains("\"index\":1"), "{started}");
        assert!(ev.iter().any(|e| e.contains("input_json_delta")));
        // Second (text) delta still lands on index 0, not on the tool block.
        let ev = t.feed(&serde_json::json!({"choices": [{"delta": {"content": "x"}}]}));
        assert!(ev
            .iter()
            .any(|e| e.contains("\"index\":0") && e.contains("text_delta")));
    }

    #[test]
    fn stream_translator_indices_are_monotonic() {
        let mut t = StreamTranslator::new("gw");
        let _ = t.prefix();
        let ev = t.feed(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "a", "function": {"name": "Read", "arguments": "{}"}},
                {"index": 1, "id": "b", "function": {"name": "Bash", "arguments": "{}"}}
            ]}}]
        }));
        let indices: Vec<usize> = ev
            .iter()
            .filter(|e| e.contains("content_block_start"))
            .filter_map(|e| {
                let json = e.split_once("data: ").map(|(_, d)| d.trim())?;
                serde_json::from_str::<Value>(json)
                    .ok()?
                    .get("index")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
            })
            .collect();
        assert_eq!(indices, vec![0, 1, 2], "{ev:?}");
        assert!(indices.windows(2).all(|w| w[0] < w[1]));
    }
}