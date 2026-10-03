//! Responses API translation (Anthropic <-> OpenAI `/responses`).

use super::common::{floor_output_tokens, new_message_id};
use super::shared::{block_text, image_part_to_responses, tool_result_parts};
use super::sse::sse;
use super::sse::sse_error;
use serde_json::Value;

pub fn anthropic_to_responses(body: &Value, upstream_model: &str) -> Value {
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
                                calls.push(serde_json::json!({
                                    "type": "function_call",
                                    "call_id": b.get("id").cloned().unwrap_or(Value::String("call_0".into())),
                                    "name": b.get("name").cloned().unwrap_or(Value::String("".into())),
                                    "arguments": serde_json::to_string(b.get("input").unwrap_or(&Value::Object(Default::default()))).unwrap_or_else(|_| "{}".into())
                                }));
                            }
                            Some("tool_result") => {
                                let id = b
                                    .get("tool_use_id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
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

fn parse_args(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(s).unwrap_or(Value::Object(Default::default()))
}

pub fn responses_to_anthropic(resp: &Value, gateway_model: &str) -> Value {
    let msg_id = new_message_id();
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

/// Stateful translator: OpenAI Responses SSE -> Anthropic SSE event lines.
#[derive(Debug, Default)]

pub struct ResponsesTranslator {
    pub gateway_model: String,
    pub msg_id: String,
    pub text_open: bool,
    pub text_closed: bool,
    pub tool_blocks: Vec<ResponsesToolBlock>,
    pub message_started: bool,
    pub input_tokens: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ResponsesToolBlock {
    pub key: String,
    pub block_index: usize,
    pub id: String,
    pub name: String,
    pub started: bool,
    pub closed: bool,
}

impl ResponsesTranslator {
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

    fn tool_block(&mut self, key: &str) -> usize {
        if let Some(pos) = self.tool_blocks.iter().position(|b| b.key == key) {
            return pos;
        }
        let block_index = 1 + self.tool_blocks.len();
        self.tool_blocks.push(ResponsesToolBlock {
            key: key.to_string(),
            block_index,
            ..Default::default()
        });
        self.tool_blocks.len() - 1
    }

    fn start_tool(&mut self, pos: usize, out: &mut Vec<String>) {
        if self.tool_blocks[pos].started {
            return;
        }
        if !self.text_open {
            self.ensure_text(out);
        }
        if self.tool_blocks[pos].id.is_empty() {
            self.tool_blocks[pos].id = format!("toolu_{pos}");
        }
        self.tool_blocks[pos].started = true;
        let b = &self.tool_blocks[pos];
        out.push(sse(&serde_json::json!({
            "type": "content_block_start", "index": b.block_index,
            "content_block": {"type": "tool_use", "id": b.id, "name": b.name, "input": {}}
        })));
    }

    /// Feed one parsed Responses event payload, return Anthropic `data:` lines.
    pub fn feed(&mut self, ev: &Value) -> Vec<String> {
        let mut out = vec![];
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("response.output_text.delta") => {
                if let Some(t) = ev.get("delta").and_then(|d| d.as_str()) {
                    if !t.is_empty() {
                        self.ensure_text(&mut out);
                        out.push(sse(&serde_json::json!({
                            "type": "content_block_delta", "index": 0,
                            "delta": {"type": "text_delta", "text": t}
                        })));
                    }
                }
            }
            Some("response.output_item.added") => {
                if let Some(item) = ev.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let key = ev
                            .get("output_index")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| self.tool_blocks.len().to_string());
                        let pos = self.tool_block(&key);
                        if let Some(id) = item.get("call_id").and_then(|x| x.as_str()) {
                            if !id.is_empty() {
                                self.tool_blocks[pos].id = id.to_string();
                            }
                        }
                        if let Some(name) = item.get("name").and_then(|x| x.as_str()) {
                            self.tool_blocks[pos].name = name.to_string();
                        }
                        self.start_tool(pos, &mut out);
                    }
                }
            }
            Some("response.function_call_arguments.delta") => {
                let key = ev
                    .get("output_index")
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "0".to_string());
                let pos = self.tool_block(&key);
                self.start_tool(pos, &mut out);
                if let Some(args) = ev.get("delta").and_then(|d| d.as_str()) {
                    if !args.is_empty() {
                        let bi = self.tool_blocks[pos].block_index;
                        out.push(sse(&serde_json::json!({
                            "type": "content_block_delta", "index": bi,
                            "delta": {"type": "input_json_delta", "partial_json": args}
                        })));
                    }
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = ev.get("item") {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let key = ev
                            .get("output_index")
                            .map(|i| i.to_string())
                            .unwrap_or_default();
                        if let Some(pos) = self.tool_blocks.iter().position(|b| b.key == key) {
                            if let Some(id) = item.get("call_id").and_then(|x| x.as_str()) {
                                if !id.is_empty() {
                                    self.tool_blocks[pos].id = id.to_string();
                                }
                            }
                            if let Some(name) = item.get("name").and_then(|x| x.as_str()) {
                                if !name.is_empty() {
                                    self.tool_blocks[pos].name = name.to_string();
                                }
                            }
                            self.tool_blocks[pos].closed = true;
                            let bi = self.tool_blocks[pos].block_index;
                            out.push(sse(
                                &serde_json::json!({"type": "content_block_stop", "index": bi}),
                            ));
                        }
                    }
                }
            }
            // The upstream reports a mid-stream failure. Surface it as an
            // Anthropic `error` event; the caller also appends one after
            // `message_stop` for `response.failed`.
            Some("error") => {
                let e = ev.get("error").unwrap_or(ev);
                let msg = e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream error");
                let ty = e
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("api_error");
                out.push(sse_error(ty, msg));
            }
            _ => {}
        }
        out
    }

    pub fn has_tools(&self) -> bool {
        self.tool_blocks.iter().any(|b| b.started)
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
                    &serde_json::json!({"type": "content_block_stop", "index": b.block_index}),
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

// ---------------------------------------------------------------------------
// OpenAI SSE chunk -> Anthropic SSE event lines
// ---------------------------------------------------------------------------

/// Stateful translator for one streaming response.
#[derive(Debug, Default)]

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CatalogEntry, CatalogSettings};

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
    fn responses_stream_translator_text_and_tool() {
        let mut t = ResponsesTranslator::new("gw");
        assert_eq!(t.prefix().len(), 1);
        let d1 = serde_json::json!({"type": "response.output_text.delta", "delta": "hi"});
        assert!(t.feed(&d1).iter().any(|e| e.contains("text_delta")));
        let added = serde_json::json!({
            "type": "response.output_item.added", "output_index": 1,
            "item": {"type": "function_call", "call_id": "c1", "name": "Read"}
        });
        assert!(t.feed(&added).iter().any(|e| e.contains("tool_use")));
        let d2 = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1, "delta": "{\"p\":"
        });
        assert!(t.feed(&d2).iter().any(|e| e.contains("input_json_delta")));
        assert!(t.has_tools());
        let end = t.finish("tool_use", 0, 3);
        assert!(end.iter().any(|e| e.contains("message_stop")));
    }

    #[test]
    fn responses_error_event_is_surfaced() {
        let mut tr = ResponsesTranslator::new("gw");
        let _ = tr.prefix();
        let out = tr.feed(&serde_json::json!({
            "type": "error",
            "error": {"type": "api_error", "message": "overloaded"}
        }));
        assert!(out.iter().any(|e| e.contains("event: error")));
        assert!(out.iter().any(|e| e.contains("overloaded")));
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

    #[test]
    fn responses_stream_usage_lands_in_message_delta() {
        let mut t = ResponsesTranslator::new("gw");
        let _ = t.prefix();
        t.input_tokens = 11;
        let end = t.finish("end_turn", 11, 4);
        let delta = end
            .iter()
            .find(|e| e.contains("message_delta"))
            .expect("message_delta");
        assert!(delta.contains("\"input_tokens\":11"), "{delta}");
        assert!(delta.contains("\"output_tokens\":4"), "{delta}");
    }

    // Short real-time durations (no paused clock needed).
}