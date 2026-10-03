//! Upstream: forward to OpenCode Zen/provider endpoints.
//!
//! - `anthropic` package: byte passthrough to `{baseURL}/messages`
//!   (forward `anthropic-version` / `anthropic-beta` / body unchanged).
//! - `openai*` packages: translate Anthropic <-> OpenAI Chat Completions.

pub mod chat;
pub mod common;
pub mod responses;
pub mod shared;
pub mod sse;
pub mod tokens;
pub mod variant;

// Compatibility surface: everything the rest of the crate (and the
// integration tests) imported from `infra::upstream` keeps resolving.
// New code should import from the specific submodule instead.
pub use chat::{anthropic_to_openai, openai_to_anthropic, StreamTranslator, ToolBlock};
pub use common::{join_url, new_message_id};
pub use responses::{
    anthropic_to_responses, responses_to_anthropic, ResponsesToolBlock, ResponsesTranslator,
};
pub use sse::{sse, sse_error, with_heartbeat};
pub use tokens::estimate_tokens;
pub use variant::{
    apply_variant, apply_variant_checked, normalize_reasoning, variant_plan, AnthropicVariantError,
    VariantPlan,
};
