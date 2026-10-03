//! Characterization tests (Fase 0 of the maintainability plan).
//!
//! These lock the *observable* behavior of the pure translators, the variant
//! plans, the protocol table and the alias builders before any refactor
//! touches them. If a later phase changes an assertion here, that change must
//! be deliberate and reviewed — never incidental.
//!
//! End-to-end coverage (mock upstream, all three wire protocols, streaming,
//! beta merge, count_tokens) lives in `tests/gateway.rs`.

use frank_opencode::domain::{
    auto_aliases_for, evade_desktop_blocklist, is_known_package, protocol_for, protocol_for_entry,
    strip_window_suffix, window_suffix, CatalogEntry, CatalogSettings, ModelVariant,
    ModelVariantSettings, Protocol, ThinkingConfig,
};
use frank_opencode::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, apply_variant_checked,
    estimate_tokens, join_url, normalize_reasoning, openai_to_anthropic, responses_to_anthropic,
    sse, sse_error, variant_plan, ResponsesTranslator, StreamTranslator, VariantPlan,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn catalog_row(provider: &str, id: &str, model: &str, package: &str) -> CatalogEntry {
    CatalogEntry {
        id: id.to_string(),
        model_id: model.to_string(),
        provider_id: provider.to_string(),
        name: model.to_string(),
        package: package.to_string(),
        settings: CatalogSettings::default(),
        limit: None,
        enabled: true,
        variants: vec![],
        headers: None,
        body: None,
    }
}

fn copilot_entry(endpoint: Option<&str>) -> CatalogEntry {
    let mut e = catalog_row("github-copilot", "m", "m", "aisdk:@ai-sdk/github-copilot");
    e.settings.endpoint = endpoint.map(|s| s.to_string());
    e
}

fn reasoning_variant(id: &str, effort: &str) -> ModelVariant {
    ModelVariant {
        id: id.to_string(),
        settings: ModelVariantSettings {
            reasoning_effort: Some(effort.to_string()),
            ..Default::default()
        },
    }
}

fn thinking_variant(id: &str, thinking: ThinkingConfig) -> ModelVariant {
    ModelVariant {
        id: id.to_string(),
        settings: ModelVariantSettings {
            thinking: Some(thinking),
            ..Default::default()
        },
    }
}

/// `msg_<24 lowercase hex>`: the exact shape every translator emits today.
/// Fase 1 unifies the four call sites behind one helper; this locks the
/// format that helper must preserve.
fn assert_id_shape(id: &Value) {
    let s = id.as_str().expect("message id is a string");
    assert!(s.starts_with("msg_"), "{s}");
    assert_eq!(s.len(), 28, "{s}");
    assert!(s[4..].chars().all(|c| c.is_ascii_hexdigit()), "{s}");
}

/// Split one `sse()` frame into its event name and parsed `data:` payload.
/// Payloads are compared as `Value` so JSON key order never matters.
fn parse_frame(line: &str) -> (String, Value) {
    let mut event = String::new();
    let mut data = String::new();
    for l in line.lines() {
        if let Some(e) = l.strip_prefix("event: ") {
            event = e.to_string();
        }
        if let Some(d) = l.strip_prefix("data: ") {
            data = d.to_string();
        }
    }
    assert!(!event.is_empty(), "{line}");
    let payload: Value = serde_json::from_str(&data).expect("data: is json");
    (event, payload)
}

// ---------------------------------------------------------------------------
// Protocol table
// ---------------------------------------------------------------------------

#[test]
fn protocol_table_routes_packages() {
    let cases = [
        (
            "@opencode/ai/providers/anthropic",
            Protocol::Anthropic,
            true,
        ),
        (
            "@opencode/ai/providers/anthropic-compatible",
            Protocol::Anthropic,
            true,
        ),
        (
            "@opencode/ai/providers/openai",
            Protocol::Responses,
            true,
        ),
        (
            "@opencode/ai/providers/openai/responses",
            Protocol::Responses,
            true,
        ),
        (
            "@opencode/ai/providers/openai-compatible",
            Protocol::ChatCompletions,
            true,
        ),
        (
            "@opencode/ai/providers/openrouter",
            Protocol::ChatCompletions,
            true,
        ),
        ("@acme/custom-thing", Protocol::ChatCompletions, false),
    ];
    for (pkg, proto, known) in cases {
        assert_eq!(protocol_for(pkg), proto, "{pkg}");
        assert_eq!(is_known_package(pkg), known, "{pkg}");
    }
}

#[test]
fn protocol_for_entry_honors_copilot_endpoint() {
    assert_eq!(
        protocol_for_entry(&copilot_entry(Some("responses"))),
        Protocol::Responses
    );
    assert_eq!(
        protocol_for_entry(&copilot_entry(Some("chat"))),
        Protocol::ChatCompletions
    );
    assert_eq!(
        protocol_for_entry(&copilot_entry(Some("messages"))),
        Protocol::Anthropic
    );
    // Unknown/absent endpoint on aisdk falls back to ChatCompletions.
    assert_eq!(
        protocol_for_entry(&copilot_entry(Some("weird"))),
        Protocol::ChatCompletions
    );
    assert_eq!(
        protocol_for_entry(&copilot_entry(None)),
        Protocol::ChatCompletions
    );
    // Outside aisdk the endpoint label is ignored.
    let mut e = catalog_row(
        "github-copilot",
        "c",
        "claude-sonnet-5.5",
        "@opencode/ai/providers/anthropic",
    );
    e.settings.endpoint = Some("chat".to_string());
    assert_eq!(protocol_for_entry(&e), Protocol::Anthropic);
}

// ---------------------------------------------------------------------------
// Chat Completions translation
// ---------------------------------------------------------------------------

#[test]
fn chat_translation_conversation_shape() {
    let body = json!({
        "model": "gw-x",
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
        "tool_choice": {"type": "auto"},
        "temperature": 0.5,
        "max_tokens": 64,
        "stop_sequences": ["END"]
    });
    let out = anthropic_to_openai(&body, "up-model");
    assert_eq!(out["model"], "up-model");
    let messages = out["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0], json!({"role": "system", "content": "be concise"}));
    assert_eq!(messages[1], json!({"role": "user", "content": "hi"}));
    // Assistant turn keeps its text and carries the tool calls.
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "looking");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "t1");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "Read");
    let args = messages[2]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(args).unwrap(),
        json!({"path": "a"})
    );
    // Each tool_result becomes its own `tool` message.
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "t1");
    assert_eq!(messages[3]["content"], "ok");
    // Tools, choice, sampling and stop are carried over.
    assert_eq!(out["tools"][0]["function"]["name"], "Read");
    assert_eq!(out["tool_choice"], "auto");
    assert_eq!(out["temperature"], 0.5);
    assert_eq!(out["max_tokens"], 64);
    assert_eq!(out["stop"], json!(["END"]));
}

#[test]
fn chat_tool_choice_mapping() {
    let cases = [
        (json!({"type": "any"}), json!("required")),
        (json!({"type": "none"}), json!("none")),
        (
            json!({"type": "tool", "name": "Bash"}),
            json!({"type": "function", "function": {"name": "Bash"}}),
        ),
        (json!({"type": "tool"}), json!("required")),
        (json!({"type": "auto"}), json!("auto")),
        (json!({"type": "weird"}), json!("auto")),
    ];
    for (choice, expected) in cases {
        let body = json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Bash"}],
            "tool_choice": choice,
        });
        let out = anthropic_to_openai(&body, "up");
        assert_eq!(out["tool_choice"], expected);
    }
}

#[test]
fn chat_stream_flag_and_output_floor() {
    // Claude Code's model-switch probe sends `max_tokens: 1`; the zen backend
    // rejects output limits below 16, so the gateway floors it.
    let probe = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": true
    });
    let out = anthropic_to_openai(&probe, "up");
    assert_eq!(out["max_tokens"], 16);
    assert_eq!(out["stream"], true);
    assert_eq!(out["stream_options"], json!({"include_usage": true}));
    // Non-streaming requests carry no stream keys at all.
    let plain = json!({
        "model": "x",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64
    });
    let out = anthropic_to_openai(&plain, "up");
    assert_eq!(out["max_tokens"], 64);
    assert!(out.get("stream").is_none());
    assert!(out.get("stream_options").is_none());
}

#[test]
fn chat_failed_tool_result_is_explicit() {
    // Chat Completions has no error flag on tool outputs, so failures are
    // made explicit in the payload text instead of being flattened silently.
    let body = json!({
        "model": "x",
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t", "content": "boom", "is_error": true}
        ]}]
    });
    let out = anthropic_to_openai(&body, "up");
    assert_eq!(out["messages"][0]["content"], "Error: boom");
    let body = json!({
        "model": "x",
        "messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t", "is_error": true}
        ]}]
    });
    let out = anthropic_to_openai(&body, "up");
    assert_eq!(
        out["messages"][0]["content"],
        "Error: tool execution failed"
    );
}

// ---------------------------------------------------------------------------
// Responses translation
// ---------------------------------------------------------------------------

#[test]
fn responses_translation_conversation_shape() {
    let body = json!({
        "model": "gw-x",
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
        "tool_choice": {"type": "auto"},
        "temperature": 0.5,
        "max_tokens": 64
    });
    let out = anthropic_to_responses(&body, "up-model");
    assert_eq!(out["model"], "up-model");
    assert_eq!(out["instructions"], "be concise");
    assert_eq!(out["max_output_tokens"], 64);
    let input = out["input"].as_array().unwrap();
    assert_eq!(input[0]["content"][0], json!({"type": "input_text", "text": "hi"}));
    assert_eq!(
        input[1]["content"][0],
        json!({"type": "output_text", "text": "looking"})
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "t1");
    assert_eq!(input[2]["name"], "Read");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "t1");
    assert_eq!(input[3]["output"], "ok");
    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["name"], "Read");
    assert_eq!(out["tool_choice"], "auto");
    assert_eq!(out["temperature"], 0.5);
    assert!(out.get("stream").is_none());
}

// ---------------------------------------------------------------------------
// Reverse translation (upstream -> Anthropic)
// ---------------------------------------------------------------------------

#[test]
fn openai_response_mapping_with_tools() {
    let resp = json!({
        "id": "chatcmpl-1",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": "working",
                "tool_calls": [{
                    "id": "call_9",
                    "function": {"name": "Bash", "arguments": "{\"cmd\":\"ls\"}"}
                }]
            }
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7}
    });
    let a = openai_to_anthropic(&resp, "gw");
    assert_eq!(a["model"], "gw");
    assert_eq!(a["content"][0], json!({"type": "text", "text": "working"}));
    assert_eq!(a["content"][1]["type"], "tool_use");
    assert_eq!(a["content"][1]["id"], "call_9");
    assert_eq!(a["content"][1]["name"], "Bash");
    assert_eq!(a["content"][1]["input"], json!({"cmd": "ls"}));
    assert_eq!(a["stop_reason"], "tool_use");
    assert_eq!(a["usage"], json!({"input_tokens": 5, "output_tokens": 7}));
    assert_id_shape(&a["id"]);
}

#[test]
fn openai_response_length_and_empty_shapes() {
    // `length` finish without tools means the output was cut.
    let resp = json!({
        "choices": [{"finish_reason": "length", "message": {"content": "cut"}}],
        "usage": {}
    });
    let a = openai_to_anthropic(&resp, "gw");
    assert_eq!(a["stop_reason"], "max_tokens");
    assert_eq!(a["content"][0]["text"], "cut");
    // No choices at all still yields a well-formed empty message.
    let resp = json!({});
    let a = openai_to_anthropic(&resp, "gw");
    assert_eq!(a["stop_reason"], "end_turn");
    assert_eq!(a["content"], json!([{"type": "text", "text": ""}]));
    assert_eq!(a["usage"], json!({"input_tokens": 0, "output_tokens": 0}));
}

#[test]
fn responses_response_mapping_with_tools() {
    let resp = json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
            {"type": "function_call", "call_id": "c2", "name": "Bash",
             "arguments": "{\"cmd\":\"ls\"}"}
        ],
        "usage": {"input_tokens": 5, "output_tokens": 7}
    });
    let a = responses_to_anthropic(&resp, "gw");
    assert_eq!(a["model"], "gw");
    assert_eq!(a["content"][0], json!({"type": "text", "text": "done"}));
    assert_eq!(a["content"][1]["type"], "tool_use");
    assert_eq!(a["content"][1]["id"], "c2");
    assert_eq!(a["content"][1]["input"], json!({"cmd": "ls"}));
    assert_eq!(a["stop_reason"], "tool_use");
    assert_eq!(a["usage"], json!({"input_tokens": 5, "output_tokens": 7}));
    assert_id_shape(&a["id"]);
}

#[test]
fn responses_response_incomplete_and_empty_shapes() {
    let resp = json!({
        "id": "r",
        "status": "incomplete",
        "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 2}
    });
    let a = responses_to_anthropic(&resp, "gw");
    assert_eq!(a["stop_reason"], "max_tokens");
    assert_eq!(a["content"], json!([{"type": "text", "text": ""}]));
}

// ---------------------------------------------------------------------------
// Variant plans
// ---------------------------------------------------------------------------

#[test]
fn reasoning_label_clamps_are_safety_critical() {
    // `none` must survive so it can never become maximum reasoning by
    // accident; `max` has no OpenAI equivalent and degrades to `high`.
    assert_eq!(normalize_reasoning("none"), "none");
    assert_eq!(normalize_reasoning("MAX"), "high");
    assert_eq!(normalize_reasoning("garbage"), "high");
}

#[test]
fn variant_plan_chat_completions() {
    let mut e = catalog_row("p", "m", "m", "@opencode/ai/providers/openai-compatible");
    e.variants = vec![
        reasoning_variant("high", "high"),
        reasoning_variant("off", "none"),
        reasoning_variant("top", "MAX"),
    ];
    assert_eq!(
        variant_plan(&e, "high"),
        Ok(VariantPlan::ChatReasoning("high".to_string()))
    );
    // `MAX` clamps to `high` (see `reasoning_label_clamps_are_safety_critical`).
    assert_eq!(
        variant_plan(&e, "top"),
        Ok(VariantPlan::ChatReasoning("high".to_string()))
    );
    // Explicit `none` removes the key instead of sending a bogus value.
    let out = apply_variant(json!({"reasoning_effort": "low"}), &e, "off");
    assert!(out.get("reasoning_effort").is_none());
    // Unknown labels pass through as no-ops (callers validate separately).
    assert_eq!(variant_plan(&e, "ghost"), Ok(VariantPlan::None));
}

#[test]
fn variant_plan_responses_carries_include() {
    let mut e = catalog_row("p", "m", "m", "@opencode/ai/providers/openai");
    e.variants = vec![ModelVariant {
        id: "low".to_string(),
        settings: ModelVariantSettings {
            reasoning_effort: Some("low".to_string()),
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
            ..Default::default()
        },
    }];
    assert_eq!(
        variant_plan(&e, "low"),
        Ok(VariantPlan::ResponsesReasoning {
            effort: "low".to_string(),
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
        })
    );
    let out = apply_variant(json!({}), &e, "low");
    assert_eq!(out["reasoning"], json!({"effort": "low"}));
    assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn variant_plan_anthropic_thinking_and_rejection() {
    let mut e = catalog_row("p", "m", "m", "@opencode/ai/providers/anthropic");
    e.variants = vec![
        thinking_variant(
            "calm",
            ThinkingConfig::Typed {
                kind: "disabled".to_string(),
                display: None,
            },
        ),
        thinking_variant("deep", ThinkingConfig::Label("high".to_string())),
        reasoning_variant("xhigh", "xhigh"),
    ];
    assert_eq!(
        variant_plan(&e, "calm"),
        Ok(VariantPlan::AnthropicThinking {
            thinking: Value::Null
        })
    );
    let out = apply_variant_checked(json!({"model": "m"}), &e, "calm").unwrap();
    assert_eq!(out["thinking"], json!({"type": "disabled"}));
    // Any non-disabled thinking becomes adaptive (display fixed by catalog).
    let out = apply_variant_checked(json!({}), &e, "deep").unwrap();
    assert_eq!(
        out["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    // `reasoningEffort` alone has no Messages API representation: a 400,
    // never a silent no-op.
    assert!(apply_variant_checked(json!({}), &e, "xhigh").is_err());
}

// ---------------------------------------------------------------------------
// SSE framing and full translator sequences
// ---------------------------------------------------------------------------

#[test]
fn sse_frames_carry_event_and_data() {
    let line = sse(&json!({"type": "ping"}));
    assert!(line.starts_with("event: ping\n"), "{line}");
    assert!(line.ends_with("\n\n"), "{line}");
    let (event, payload) = parse_frame(&line);
    assert_eq!(event, "ping");
    assert_eq!(payload, json!({"type": "ping"}));
    // A payload without `type` degrades to a generic message event.
    let (event, _) = parse_frame(&sse(&json!({"x": 1})));
    assert_eq!(event, "message");
    let (event, payload) = parse_frame(&sse_error("api_error", "boom"));
    assert_eq!(event, "error");
    assert_eq!(payload["error"]["type"], "api_error");
    assert_eq!(payload["error"]["message"], "boom");
}

#[test]
fn chat_stream_translator_full_sequence() {
    let mut t = StreamTranslator::new("gw-model");
    let mut frames = t.prefix();
    frames.extend(t.feed(&json!({"choices": [{"delta": {"content": "hel"}}]})));
    frames.extend(t.feed(&json!({"choices": [{"delta": {"content": "lo"}}]})));
    frames.extend(t.finish("end_turn", 3, 12));
    assert_eq!(frames.len(), 7);
    let parsed: Vec<(String, Value)> = frames.iter().map(|f| parse_frame(f)).collect();
    let events: Vec<&str> = parsed.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(
        events.join(","),
        "message_start,content_block_start,content_block_delta,\
         content_block_delta,content_block_stop,message_delta,message_stop"
    );
    assert_eq!(parsed[0].1["message"]["model"], "gw-model");
    assert_id_shape(&parsed[0].1["message"]["id"]);
    assert_eq!(parsed[1].1["index"], 0);
    assert_eq!(parsed[1].1["content_block"]["type"], "text");
    assert_eq!(parsed[2].1["delta"]["text"], "hel");
    assert_eq!(parsed[3].1["delta"]["text"], "lo");
    assert_eq!(parsed[4].1["index"], 0);
    assert_eq!(parsed[5].1["delta"]["stop_reason"], "end_turn");
    assert_eq!(parsed[5].1["usage"]["input_tokens"], 3);
    assert_eq!(parsed[5].1["usage"]["output_tokens"], 12);
}

#[test]
fn responses_stream_translator_full_sequence() {
    let mut t = ResponsesTranslator::new("gw-model");
    let mut frames = t.prefix();
    frames.extend(t.feed(
        &json!({"type": "response.output_text.delta", "delta": "hi"}),
    ));
    frames.extend(t.feed(&json!({
        "type": "response.output_item.added",
        "output_index": 1,
        "item": {"type": "function_call", "call_id": "c1", "name": "Read"}
    })));
    frames.extend(t.feed(&json!({
        "type": "response.function_call_arguments.delta",
        "output_index": 1,
        "delta": "{\"p\":1}"
    })));
    frames.extend(t.feed(&json!({
        "type": "response.output_item.done",
        "output_index": 1,
        "item": {"type": "function_call", "call_id": "c1", "name": "Read"}
    })));
    frames.extend(t.finish("tool_use", 7, 9));
    assert_eq!(frames.len(), 9);
    let parsed: Vec<(String, Value)> = frames.iter().map(|f| parse_frame(f)).collect();
    let events: Vec<&str> = parsed.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(
        events.join(","),
        "message_start,content_block_start,content_block_delta,\
         content_block_start,content_block_delta,content_block_stop,\
         content_block_stop,message_delta,message_stop"
    );
    // The tool block opens at index 1 with the upstream id/name.
    assert_eq!(parsed[3].1["index"], 1);
    assert_eq!(parsed[3].1["content_block"]["type"], "tool_use");
    assert_eq!(parsed[3].1["content_block"]["id"], "c1");
    assert_eq!(parsed[3].1["content_block"]["name"], "Read");
    assert_eq!(parsed[4].1["delta"]["partial_json"], "{\"p\":1}");
    assert_eq!(parsed[7].1["delta"]["stop_reason"], "tool_use");
    assert_eq!(parsed[7].1["usage"]["input_tokens"], 7);
    assert_eq!(parsed[7].1["usage"]["output_tokens"], 9);
}

// ---------------------------------------------------------------------------
// Tokens, URLs, aliases
// ---------------------------------------------------------------------------

#[test]
fn estimate_tokens_is_deterministic_on_fixed_bodies() {
    let body = json!({"messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(estimate_tokens(&body), 4);
    let rich = json!({
        "system": "abcd",
        "messages": [{"role": "user", "content": "hello world"}],
        "tools": [{
            "name": "R",
            "description": "d",
            "input_schema": {"type": "object"}
        }]
    });
    assert_eq!(estimate_tokens(&rich), 19);
}

#[test]
fn join_url_trims_slashes_on_both_sides() {
    assert_eq!(join_url("http://x/", "/messages"), "http://x/messages");
    assert_eq!(join_url("http://x", "messages"), "http://x/messages");
    assert_eq!(join_url("http://x//", "//m"), "http://x/m");
}

#[test]
fn auto_aliases_keep_collision_and_fast_rows_distinct() {
    // `v4.1` vs `v4-1` slug to the same id; the first row keeps the base
    // alias and the second gets a numeric suffix — both must survive.
    let a = catalog_row("opencode-go", "deepseek-v4.1-flash", "deepseek-v4.1-flash", "p");
    let b = catalog_row("opencode-go", "deepseek-v4-1-flash", "deepseek-v4-1-flash", "p");
    let aliases = auto_aliases_for(&[a, b], false);
    assert_eq!(aliases.len(), 2);
    assert_eq!(
        aliases[0].gateway_id,
        "claude-opencode-go-deepseek-v4-1-flash"
    );
    assert_eq!(
        aliases[1].gateway_id,
        "claude-opencode-go-deepseek-v4-1-flash-2"
    );
    // A row with a distinct `id` (fast flavor) keeps its own ref.
    let fast = catalog_row(
        "github-copilot",
        "claude-opus-4.8-fast",
        "claude-opus-4.8",
        "p",
    );
    assert_eq!(
        fast.preferred_ref(),
        "github-copilot/claude-opus-4.8-fast"
    );
}

#[test]
fn evaded_aliases_hide_blocked_token_but_keep_ref() {
    let e = catalog_row(
        "opencode-go",
        "deepseek-v4.1-flash",
        "deepseek-v4.1-flash",
        "p",
    );
    assert_eq!(evade_desktop_blocklist("deepseek-v4.1-flash"), "d-eepseek-v4.1-flash");
    let aliases = auto_aliases_for(std::slice::from_ref(&e), true);
    assert_eq!(aliases.len(), 1);
    assert!(!aliases[0].gateway_id.to_lowercase().contains("deepseek"));
    assert!(aliases[0].gateway_id.contains("claude"));
    assert_eq!(aliases[0].opencode_ref, "opencode-go/deepseek-v4.1-flash");
}

#[test]
fn window_suffix_round_trip() {
    assert_eq!(window_suffix(1_000_000), Some("[1m]".to_string()));
    assert_eq!(window_suffix(999_999), None);
    assert_eq!(strip_window_suffix("claude-x[1m]"), "claude-x");
    assert_eq!(strip_window_suffix("claude-x"), "claude-x");
}
