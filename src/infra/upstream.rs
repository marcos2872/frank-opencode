//! Upstream translators: Anthropic <-> OpenAI Chat Completions, Responses API,
//! streaming translators, variants, heartbeat and token estimates.
//!
//! Pure functions and translators (no HTTP); the wire split lives in the
//! submodules, re-exported here so `crate::infra::upstream::{...}` keeps
//! working.

pub mod chat;
pub mod estimate;
pub mod heartbeat;
pub mod responses;
pub(crate) mod shared;
pub mod stream;
pub mod variant;

pub use chat::{anthropic_to_openai, openai_to_anthropic};
pub use estimate::estimate_tokens;
pub use heartbeat::{sse, sse_error, with_heartbeat};
pub use responses::{anthropic_to_responses, responses_to_anthropic};
pub use stream::{ResponsesTranslator, StreamTranslator};
pub use variant::{
    apply_variant, apply_variant_checked, normalize_reasoning, variant_plan, AnthropicVariantError,
    VariantPlan,
};

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
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
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
}
