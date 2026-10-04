//! Streaming translators: Responses SSE and Chat SSE -> Anthropic SSE.

use super::heartbeat::{sse, sse_error};
use serde_json::Value;

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
    /// `arguments` seen before the block opened (name not known yet). Flushed
    /// as one `input_json_delta` when the name arrives.
    pub pending_args: String,
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
                if let Some(name) = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|x| x.as_str())
                {
                    if !name.is_empty() && block.name.is_empty() {
                        block.name = name.to_string();
                    }
                }
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str());
                if !block.started {
                    // Anthropic `tool_use` requires a non-empty name, so never
                    // open the block until the name is known even when the id or
                    // arguments arrive first. Arguments seen beforehand are
                    // buffered; opening here flushes them in order. If the
                    // upstream never names the tool, `finish` drops the block so
                    // the client never records a nameless `tool_use` (which a
                    // strict upstream would reject as `tool_calls[].function.name`
                    // on the next turn).
                    if !block.name.is_empty() {
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
                        if !block.pending_args.is_empty() {
                            let partial = std::mem::take(&mut block.pending_args);
                            out.push(sse(&serde_json::json!({
                                "type": "content_block_delta", "index": bi,
                                "delta": {"type": "input_json_delta", "partial_json": partial}
                            })));
                        }
                    } else if let Some(a) = args {
                        if !a.is_empty() {
                            block.pending_args.push_str(a);
                        }
                    }
                } else if let Some(a) = args {
                    if !a.is_empty() {
                        let bi = block.index;
                        out.push(sse(&serde_json::json!({
                            "type": "content_block_delta", "index": bi,
                            "delta": {"type": "input_json_delta", "partial_json": a}
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
            // Blocks never started (a nameless `tool_use` we refused to open)
            // emit nothing: closing a block that never began is invalid.
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
    use pretty_assertions::assert_eq;
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
    fn stream_translator_buffers_args_until_name_arrives() {
        // Strict gateways may stream arguments before the id/name. Anthropic
        // `tool_use` requires a name, so the block must NOT open on args alone;
        // the arguments are buffered and flushed once the name shows up.
        let mut t = StreamTranslator::new("gw");
        let _ = t.prefix();
        let ev = t.feed(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"p\":"}}
            ]}}]
        }));
        assert!(
            !ev.iter().any(|e| e.contains("tool_use")),
            "nameless tool block must not open: {ev:?}"
        );
        // The name arrives (name-only delta, no args): block opens and the
        // buffered arguments are flushed right after the start.
        let ev = t.feed(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"name": "Read"}}
            ]}}]
        }));
        let started = ev
            .iter()
            .find(|e| e.contains("content_block_start") && e.contains("tool_use"))
            .expect("tool block opens once named");
        assert!(started.contains("\"index\":1"), "{started}");
        assert!(started.contains("\"name\":\"Read\""), "{started}");
        assert!(
            ev.iter()
                .any(|e| e.contains("input_json_delta") && e.contains("{\\\"p\\\":")),
            "buffered args flushed on open: {ev:?}"
        );
        // Second (text) delta still lands on index 0, not on the tool block.
        let ev = t.feed(&serde_json::json!({"choices": [{"delta": {"content": "x"}}]}));
        assert!(ev
            .iter()
            .any(|e| e.contains("\"index\":0") && e.contains("text_delta")));
    }
    #[test]
    fn stream_translator_drops_tool_never_named() {
        // If the name never arrives, the block is never opened and `finish`
        // emits no tool block at all — the client must not record a nameless
        // `tool_use` that a strict upstream would reject next turn.
        let mut t = StreamTranslator::new("gw");
        let _ = t.prefix();
        let ev = t.feed(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"arguments": "{\"a\":1}"}}
            ]}}]
        }));
        assert!(!ev.iter().any(|e| e.contains("tool_use")), "{ev:?}");
        assert!(!t.tool_blocks[0].started);
        let end = t.finish("tool_use", 0, 3);
        assert!(
            !end.iter().any(|e| e.contains("content_block_start"))
                && !end.iter().any(|e| e.contains("\"type\":\"tool_use\"")),
            "finish must not surface a nameless tool block: {end:?}"
        );
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
}
