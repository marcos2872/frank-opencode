//! Upstream: forward to OpenCode Zen/provider endpoints.
//!
//! - `anthropic` package: byte passthrough to `{baseURL}/messages`
//!   (forward `anthropic-version` / `anthropic-beta` / body unchanged).
//! - `openai*` packages: translate Anthropic <-> OpenAI Chat Completions.

use crate::domain::{protocol_for_entry, CatalogEntry, Protocol, ThinkingConfig};
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

/// Floor for `max_tokens` on translated (non-Anthropic) upstreams: the
/// OpenCode zen backend rejects output limits below 16 — e.g.
/// `muse-spark-1.3-contributor` returns 400
/// `` `max_output_tokens` The number must be `>= 16` ``. Claude Code
/// verifies a model before switching to it mid-session with a
/// `max_tokens: 1` probe, so without the floor the switch fails with 400.
/// Real requests ask for thousands of tokens; raising a sub-16 value only
/// affects probes.
const MIN_UPSTREAM_OUTPUT_TOKENS: u64 = 16;

/// Raise a client `max_tokens` below [`MIN_UPSTREAM_OUTPUT_TOKENS`] to the
/// floor; anything else (including non-integer values) passes through.
fn floor_output_tokens(v: &Value) -> Value {
    match v.as_u64() {
        Some(n) if n < MIN_UPSTREAM_OUTPUT_TOKENS => Value::from(MIN_UPSTREAM_OUTPUT_TOKENS),
        _ => v.clone(),
    }
}

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Chat Completions
// ---------------------------------------------------------------------------

fn image_part_to_openai(b: &Value) -> Option<Value> {
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

fn image_part_to_responses(b: &Value) -> Option<Value> {
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
fn tool_result_parts(b: &Value) -> (String, Vec<Value>, Vec<Value>) {
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
// Model variants
// ---------------------------------------------------------------------------

/// Normalize a variant's reasoning label into the OpenAI `reasoning_effort`
/// enum. Anthropic's `max` label has no OpenAI equivalent and degrades to
/// `high`; an explicit `none` is preserved so it cannot become maximum
/// reasoning by accident.
pub fn normalize_reasoning(label: &str) -> &str {
    match label.to_ascii_lowercase().trim() {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "max" => "high",
        _ => "high",
    }
}

/// The `thinking` object sent when a Messages-API variant turns thinking on.
/// The catalog already fixed the display mode for these variants, so the
/// gateway forwards exactly that.
const THINKING_ADAPTIVE_JSON: &str = r#"{"type":"adaptive","display":"summarized"}"#;

/// A variant that has a catalog definition but no safe representation on the
/// Messages API. Surfaced as a 400 instead of a silent no-op, so a user who
/// asked for `#xhigh` on an Anthropic model learns it was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicVariantError {
    pub model: String,
    pub variant: String,
}

impl std::fmt::Display for AnthropicVariantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "variant '{}' on '{}' has no Messages API representation \
             (thinking/effort); it cannot be applied",
            self.variant, self.model
        )
    }
}

/// What a variant resolves to for its wire protocol, or an error when the
/// target protocol has no safe representation for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VariantPlan {
    /// No settings to apply (variant unknown, or declares nothing).
    None,
    /// Chat Completions: `reasoning_effort` string, or remove the key (`none`).
    ChatReasoning(String),
    /// Responses: `reasoning.effort` plus optional `include` parts.
    ResponsesReasoning {
        effort: String,
        include: Option<Vec<String>>,
    },
    /// Messages API: `thinking` object (`null` = the variant disables it),
    /// optional `include` parts.
    AnthropicThinking { thinking: Value },
}

/// Resolve how `variant` applies to `entry`'s protocol, or `Err` when the
/// Messages API cannot represent it safely.
pub fn variant_plan(
    entry: &CatalogEntry,
    variant: &str,
) -> Result<VariantPlan, AnthropicVariantError> {
    let Some(v) = entry.variant(variant) else {
        return Ok(VariantPlan::None);
    };
    let err = || AnthropicVariantError {
        model: entry.qualified(),
        variant: variant.to_string(),
    };
    match protocol_for_entry(entry) {
        Protocol::ChatCompletions => match v.settings.reasoning_effort.as_deref() {
            Some(r) => Ok(VariantPlan::ChatReasoning(
                normalize_reasoning(r).to_string(),
            )),
            None => Ok(VariantPlan::None),
        },
        Protocol::Responses => match v.settings.reasoning_effort.as_deref() {
            Some(r) => Ok(VariantPlan::ResponsesReasoning {
                effort: normalize_reasoning(r).to_string(),
                include: v.settings.include.clone(),
            }),
            None => Ok(VariantPlan::None),
        },
        // Messages API has no `reasoningEffort` parameter. A variant carries
        // either a `thinking` object (forwarded verbatim) and/or an `effort`
        // label (mapped to the closest thinking mode); anything else is not
        // representable and is rejected rather than silently dropped.
        Protocol::Anthropic => {
            let thinking = match &v.settings.thinking {
                Some(ThinkingConfig::Typed { kind, .. })
                    if kind.eq_ignore_ascii_case("disabled") =>
                {
                    Value::Null
                }
                Some(_) => serde_json::from_str(THINKING_ADAPTIVE_JSON).unwrap_or(Value::Null),
                None => match v.settings.effort.as_deref() {
                    // Explicit no-thinking variant.
                    Some(e) if e.eq_ignore_ascii_case("none") => Value::Null,
                    Some(e) => thinking_for_effort(e),
                    // `reasoningEffort` alone on a Messages-API model was the
                    // old silent no-op; keep rejecting it.
                    None if v.settings.reasoning_effort.is_some() => return Err(err()),
                    None => return Ok(VariantPlan::None),
                },
            };
            Ok(VariantPlan::AnthropicThinking { thinking })
        }
    }
}

/// Map a Messages-API effort label onto a `thinking` object. Unknown labels
/// degrade to adaptive thinking rather than dropping the request.
fn thinking_for_effort(effort: &str) -> Value {
    match effort.to_ascii_lowercase().trim() {
        // The catalog's `thinking: {"type":"disabled"}` equivalent.
        "none" => Value::Null,
        // Anthropic accepts `thinking: {"type":"enabled"}` at a lower effort.
        "minimal" | "low" => serde_json::json!({"type": "enabled"}),
        _ => serde_json::from_str(THINKING_ADAPTIVE_JSON).unwrap_or(Value::Null),
    }
}

/// Apply a selected variant to a translated request body.
///
/// The field that carries the variant depends on the wire protocol (same
/// table as `protocol_for_entry`):
///
/// - `ChatCompletions`: `reasoning_effort` (removed for an explicit `none`).
/// - `Responses`: `reasoning.effort` plus the variant's `include` parts.
/// - `Anthropic`: `thinking` (and `include` where the catalog lists it);
///   a variant with no representable fields is rejected by
///   [`apply_variant_checked`].
///
/// Unknown variant labels pass through unchanged (callers validate).
pub fn apply_variant(body: Value, entry: &CatalogEntry, variant: &str) -> Value {
    match variant_plan(entry, variant) {
        Ok(plan) => apply_plan(body, plan),
        // Direct (unchecked) callers keep the previous lenient behavior.
        Err(_) => body,
    }
}

/// Apply a variant, surfacing a Messages-API variant the gateway cannot
/// represent (see [`variant_plan`]).
pub fn apply_variant_checked(
    body: Value,
    entry: &CatalogEntry,
    variant: &str,
) -> Result<Value, AnthropicVariantError> {
    let plan = variant_plan(entry, variant)?;
    Ok(apply_plan(body, plan))
}

fn apply_plan(mut body: Value, plan: VariantPlan) -> Value {
    match plan {
        VariantPlan::None => {}
        VariantPlan::ChatReasoning(effort) => {
            if effort == "none" {
                if let Some(o) = body.as_object_mut() {
                    o.remove("reasoning_effort");
                }
            } else {
                body["reasoning_effort"] = Value::String(effort);
            }
        }
        VariantPlan::ResponsesReasoning { effort, include } => {
            body["reasoning"] = serde_json::json!({"effort": effort});
            if let Some(include) = include {
                body["include"] = Value::Array(include.into_iter().map(Value::String).collect());
            }
        }
        VariantPlan::AnthropicThinking { thinking } => {
            match thinking {
                // The variant turns thinking off explicitly.
                Value::Null => {
                    if let Some(o) = body.as_object_mut() {
                        o.insert("thinking".into(), serde_json::json!({"type":"disabled"}));
                    }
                }
                other => {
                    body["thinking"] = other;
                }
            }
        }
    }
    body
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

pub fn sse(v: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        v.get("type").and_then(|t| t.as_str()).unwrap_or("message"),
        v
    )
}

/// Anthropic `error` SSE frame. The Messages API allows an `error` event
/// mid-stream (after `message_start`), so a translator can report an upstream
/// failure instead of ending the stream silently.
pub fn sse_error(err_type: &str, message: &str) -> String {
    sse(&serde_json::json!({
        "type": "error",
        "error": {"type": err_type, "message": message}
    }))
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

/// Per-part token estimate for the optional count_tokens endpoint (no
/// tokenizer: Anthropic itself documents its counts as an estimate). Text is
/// chars/4 (≈1 token per 4 ASCII chars); each message and tool adds a small
/// fixed overhead; images and base64 documents count their real size, so a
/// large attachment no longer inflates the count as if it were prose.
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
    use crate::domain::{CatalogEntry, CatalogSettings};
    use pretty_assertions::assert_eq;

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
    fn normalize_reasoning_clamps_to_openai_enum() {
        assert_eq!(normalize_reasoning("low"), "low");
        assert_eq!(normalize_reasoning("medium"), "medium");
        assert_eq!(normalize_reasoning("high"), "high");
        assert_eq!(normalize_reasoning("none"), "none");
        assert_eq!(normalize_reasoning("MAX"), "high");
        assert_eq!(normalize_reasoning("garbage"), "high");
    }

    #[test]
    fn apply_variant_sets_reasoning_effort_for_chat() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai-compatible",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        let body = serde_json::json!({"model": "x", "messages": []});
        let out = apply_variant(body, &e, "high");
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn apply_variant_uses_reasoning_key_for_responses() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai",
            vec![("low".to_string(), Some("low".to_string()))],
        );
        let body = serde_json::json!({"model": "x", "input": []});
        let out = apply_variant(body, &e, "low");
        assert_eq!(out["reasoning"]["effort"], "low");
        assert!(out["reasoning"].is_object());
    }

    #[test]
    fn anthropic_variant_without_representation_is_rejected() {
        // `reasoningEffort` alone has no Messages API parameter: surfaced as
        // an error rather than a silent no-op.
        let e = entry_with_variants(
            "@opencode/ai/providers/anthropic",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        let body = serde_json::json!({"model": "x"});
        assert!(apply_variant_checked(body.clone(), &e, "high").is_err());
        // Unknown variant label stays untouched for both entry points.
        assert_eq!(apply_variant(body.clone(), &e, "nope"), body);
        assert_eq!(
            apply_variant_checked(body.clone(), &e, "nope").unwrap(),
            body
        );
    }

    fn anthropic_variant(
        pkg: &str,
        id: &str,
        thinking: Option<crate::domain::ThinkingConfig>,
        effort: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> CatalogEntry {
        let mut e = entry_with_variants(pkg, vec![]);
        e.variants.push(crate::domain::ModelVariant {
            id: id.to_string(),
            settings: crate::domain::ModelVariantSettings {
                reasoning_effort: reasoning_effort.map(str::to_string),
                thinking,
                effort: effort.map(str::to_string),
                ..Default::default()
            },
        });
        e
    }

    #[test]
    fn anthropic_variant_maps_thinking_object_and_effort() {
        // Catalog `thinking` object is forwarded verbatim.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "high",
            Some(crate::domain::ThinkingConfig::Typed {
                kind: "adaptive".to_string(),
                display: Some("summarized".to_string()),
            }),
            None,
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "high").unwrap();
        assert_eq!(out["thinking"]["type"], "adaptive");
        assert_eq!(out["thinking"]["display"], "summarized");

        // A `disabled` variant turns thinking off explicitly.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "none",
            Some(crate::domain::ThinkingConfig::Typed {
                kind: "disabled".to_string(),
                display: None,
            }),
            None,
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "none").unwrap();
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_variant_effort_only_maps_to_thinking() {
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "high",
            None,
            Some("high"),
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "high").unwrap();
        assert_eq!(out["thinking"]["type"], "adaptive");

        // `none` disables thinking.
        let e = anthropic_variant(
            "@opencode/ai/providers/anthropic",
            "none",
            None,
            Some("none"),
            None,
        );
        let out = apply_variant_checked(serde_json::json!({"model": "x"}), &e, "none").unwrap();
        assert_eq!(out["thinking"]["type"], "disabled");
    }

    #[test]
    fn responses_variant_carries_include_parts() {
        let mut e = entry_with_variants(
            "@opencode/ai/providers/openai",
            vec![("high".to_string(), Some("high".to_string()))],
        );
        e.variants[0].settings.include = Some(vec!["reasoning.encrypted_content".to_string()]);
        let out = apply_variant(serde_json::json!({"model": "x"}), &e, "high");
        assert_eq!(out["reasoning"]["effort"], "high");
        assert_eq!(out["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn chat_variant_none_removes_reasoning_effort() {
        let e = entry_with_variants(
            "@opencode/ai/providers/openai-compatible",
            vec![("none".to_string(), Some("none".to_string()))],
        );
        let out = apply_variant(
            serde_json::json!({"model": "x", "reasoning_effort": "high"}),
            &e,
            "none",
        );
        assert!(out.get("reasoning_effort").is_none());
    }

    #[test]
    fn nameless_tool_use_is_dropped_with_its_result() {
        // A `tool_use` with no name has no valid OpenAI/Responses form; it is
        // dropped and its `tool_result` too, so the upstream never sees
        // `function.name: ""` nor an orphan tool output.
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                {"role": "user", "content": "go"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t0", "input": {}},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"p": "a"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t0", "content": "ghost"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ],
            "max_tokens": 10
        });

        let oai = anthropic_to_openai(&body, "up");
        let msgs = oai["messages"].as_array().unwrap();
        let calls = msgs
            .iter()
            .find(|m| m.get("tool_calls").is_some())
            .expect("named tool_call kept")
            .get("tool_calls")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1, "{msgs:?}");
        assert_eq!(calls[0]["function"]["name"], "Read");
        let tools: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(tools.len(), 1, "only the kept result survives: {msgs:?}");
        assert_eq!(tools[0]["tool_call_id"], "t1");

        let resp = anthropic_to_responses(&body, "up");
        let input = resp["input"].as_array().unwrap();
        let fcalls: Vec<&Value> = input
            .iter()
            .filter(|i| i["type"] == "function_call")
            .collect();
        assert_eq!(fcalls.len(), 1, "{input:?}");
        assert_eq!(fcalls[0]["name"], "Read");
        let fouts: Vec<&Value> = input
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .collect();
        assert_eq!(fouts.len(), 1, "no orphan function_call_output: {input:?}");
        assert_eq!(fouts[0]["call_id"], "t1");
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
