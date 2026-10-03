//! Shared upstream plumbing: URL joining, message ids, output floor.

use serde_json::Value;

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
pub(crate) fn floor_output_tokens(v: &Value) -> Value {
    match v.as_u64() {
        Some(n) if n < MIN_UPSTREAM_OUTPUT_TOKENS => Value::from(MIN_UPSTREAM_OUTPUT_TOKENS),
        _ => v.clone(),
    }
}

// ---------------------------------------------------------------------------
// Message IDs
// ---------------------------------------------------------------------------

/// New Anthropic message id (`msg_<24 lowercase hex>`).
///
/// Single construction site for every translator (and the classifier mock)
/// so the format cannot drift between the non-streaming converters and the
/// streaming translators.

pub fn new_message_id() -> String {
    format!(
        "msg_{}",
        &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]
    )
}

// ---------------------------------------------------------------------------
// Anthropic -> OpenAI Chat Completions
// ---------------------------------------------------------------------------
