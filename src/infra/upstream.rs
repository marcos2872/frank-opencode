//! Upstream: forward to OpenCode Zen/provider endpoints.
//!
//! - `anthropic` package: byte passthrough to `{baseURL}/messages`
//!   (forward `anthropic-version` / `anthropic-beta` / body unchanged).
//! - `openai*` packages: translate Anthropic <-> OpenAI Chat Completions.

use serde_json::Value;

// ---------------------------------------------------------------------------
// URL join
// ---------------------------------------------------------------------------

pub fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Chat Completions
// ---------------------------------------------------------------------------

fn block_text(b: &Value) -> Option<String> {
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
                    let mut tool_results: Vec<(String, String)> = vec![];
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
                                let txt = block_text(b).unwrap_or_default();
                                tool_results.push((id, txt));
                            }
                            _ => {}
                        }
                    }
                    if !tool_results.is_empty() {
                        // Each tool_result becomes its own `tool` message.
                        // Any accompanying text goes first as a user message.
                        let t = texts.join("\n");
                        if !t.trim().is_empty() {
                            messages.push(serde_json::json!({"role": "user", "content": t}));
                        }
                        for (id, txt) in tool_results {
                            messages.push(serde_json::json!({
                                "role": "tool", "tool_call_id": id, "content": txt
                            }));
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
        out["max_tokens"] = m.clone();
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

fn parse_args(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(s).unwrap_or(Value::Object(Default::default()))
}

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

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Responses API (`/responses`: Go GPT/Grok/Muse rows)
// ---------------------------------------------------------------------------

/// Convert an Anthropic `/v1/messages` body into an OpenAI `/responses` body.
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
                                if let Some(src) = b.get("source") {
                                    let mt = src
                                        .get("media_type")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("image/jpeg");
                                    let data =
                                        src.get("data").and_then(|x| x.as_str()).unwrap_or("");
                                    if !data.is_empty() {
                                        images.push(serde_json::json!({
                                            "type": "input_image",
                                            "image_url": format!("data:{mt};base64,{data}")
                                        }));
                                    }
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
                                outputs.push(serde_json::json!({
                                    "type": "function_call_output",
                                    "call_id": id,
                                    "output": block_text(b).unwrap_or_default()
                                }));
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
        out["max_output_tokens"] = m.clone();
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

/// Stateful translator: OpenAI Responses SSE -> Anthropic SSE event lines.
#[derive(Debug, Default)]
pub struct ResponsesTranslator {
    pub gateway_model: String,
    pub msg_id: String,
    pub text_open: bool,
    pub text_closed: bool,
    pub tool_blocks: Vec<ResponsesToolBlock>,
    pub message_started: bool,
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
            msg_id: format!(
                "msg_{}",
                &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
            ),
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
                "usage": {"input_tokens": 0, "output_tokens": 0}}
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
            _ => {}
        }
        out
    }

    pub fn has_tools(&self) -> bool {
        self.tool_blocks.iter().any(|b| b.started)
    }

    pub fn finish(&mut self, stop_reason: &str, output_tokens: u64) -> Vec<String> {
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
            "usage": {"output_tokens": output_tokens}
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
pub struct StreamTranslator {
    pub gateway_model: String,
    pub msg_id: String,
    pub text_open: bool,
    pub text_closed: bool,
    pub tool_blocks: Vec<ToolBlock>,
    pub message_started: bool,
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
            msg_id: format!(
                "msg_{}",
                &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
            ),
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
                "usage": {"input_tokens": 0, "output_tokens": 0}}
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
                if !block.started && (!block.name.is_empty() || !block.id.is_empty()) {
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

    pub fn finish(&mut self, stop_reason: &str, output_tokens: u64) -> Vec<String> {
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
            "usage": {"output_tokens": output_tokens}
        })));
        out.push(sse(&serde_json::json!({"type": "message_stop"})));
        out
    }
}

pub fn sse(v: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        v.get("type").and_then(|t| t.as_str()).unwrap_or("message"),
        v
    )
}

/// Wrap a byte stream, injecting `event: ping` SSE frames whenever the
/// upstream stays silent longer than `idle`. Keeps the client's stream
/// watchdog fed during long reasoning pauses.
pub fn with_heartbeat<S>(
    stream: S,
    idle: std::time::Duration,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>>
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>>,
{
    async_stream::stream! {
        let mut inner = Box::pin(stream);
        loop {
            match tokio::time::timeout(idle, futures::StreamExt::next(&mut inner)).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Ok::<_, std::io::Error>(
                        "event: ping\ndata: {\"type\": \"ping\"}\n\n".as_bytes().to_vec(),
                    );
                }
            }
        }
    }
}

/// Rough token estimate for the optional count_tokens endpoint.
pub fn estimate_tokens(body: &Value) -> u64 {
    let s = body.to_string();
    (s.len() as u64 / 4).max(1)
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
        let end = t.finish("tool_use", 3);
        assert!(end.iter().any(|e| e.contains("message_stop")));
    }

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
            "max_tokens": 10
        });
        let r = anthropic_to_responses(&body, "upstream");
        assert_eq!(r["model"], "upstream");
        assert_eq!(r["instructions"], "be concise");
        assert_eq!(r["max_output_tokens"], 10);
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
        let end = t.finish("tool_use", 3);
        assert!(end.iter().any(|e| e.contains("message_stop")));
    }

    // Short real-time durations (no paused clock needed).
    #[tokio::test]
    async fn heartbeat_fires_during_silence() {
        use futures::StreamExt;
        use std::time::Duration;
        let slow = async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(180)).await;
            yield Ok::<_, std::io::Error>(b"data".to_vec());
        };
        let mut hb = Box::pin(with_heartbeat(slow, Duration::from_millis(50)));
        // ~50/100/150ms: pings; then the payload.
        for _ in 0..3 {
            let item = hb.next().await.unwrap().unwrap();
            assert!(String::from_utf8_lossy(&item).contains("ping"));
        }
        let item = hb.next().await.unwrap().unwrap();
        assert_eq!(item, b"data".to_vec());
        assert!(hb.next().await.is_none());
    }
}
