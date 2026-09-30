//! Domain: pure types and rules. No tokio, axum, rusqlite here.

use serde::{Deserialize, Serialize};

/// Reference to a model inside OpenCode: `provider/model-id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

impl ModelRef {
    /// Parse `provider/model`, where model may itself contain `/`.
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, model) = s.split_once('/')?;
        if provider.is_empty() || model.is_empty() || provider.contains('#') {
            return None;
        }
        // Strip optional #variant suffix for resolution (variant ignored in MVP).
        let model = model.split_once('#').map(|(m, _)| m).unwrap_or(model);
        if model.is_empty() {
            return None;
        }
        Some(Self {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
        })
    }

    #[allow(dead_code)]
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

/// One model as reported by `opencode api get /api/model`.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogEntry {
    #[serde(default)]
    #[allow(dead_code)]
    pub id: String,
    #[serde(rename = "modelID", default)]
    pub model_id: String,
    #[serde(rename = "providerID", default)]
    pub provider_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub settings: CatalogSettings,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogSettings {
    #[serde(rename = "baseURL", default)]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey", default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

impl CatalogEntry {
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    pub fn is_anthropic_package(&self) -> bool {
        self.package.contains("anthropic")
    }

    /// Models served through the OpenAI Responses API (`/responses`):
    /// Go's GPT/Grok/Muse rows. Distinct from `openai-compatible` (+ `/chat`).
    pub fn is_responses_package(&self) -> bool {
        self.package == "@opencode/ai/providers/openai" || self.package.contains("openai/responses")
    }

    pub fn base_url(&self) -> Option<&str> {
        self.settings.base_url.as_deref()
    }
}

/// Gateway alias exposed on `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AliasEntry {
    /// ID seen by Claude Code, e.g. `claude-sonnet-4-6-frank`.
    pub gateway_id: String,
    /// OpenCode reference, e.g. `opencode-go/kimi-k2.7-code`.
    pub opencode_ref: String,
    pub display_name: String,
    pub description: String,
}

/// Upstream wire protocol for a provider package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Anthropic,
    ChatCompletions,
    Responses,
}

/// Map an OpenCode provider `package` to its wire protocol.
///
/// Explicit table first; unknown packages fall back to ChatCompletions with a
/// `warn!` at the call site (pass provider_id + package for the log).
pub fn protocol_for(package: &str) -> Protocol {
    if package.contains("anthropic") {
        Protocol::Anthropic
    } else if package == "@opencode/ai/providers/openai" || package.contains("openai/responses") {
        Protocol::Responses
    } else {
        Protocol::ChatCompletions
    }
}

/// Returns true when the package is not one of the known shapes, so the
/// caller can `warn!` (include provider_id + package). The gateway still
/// routes it via the ChatCompletions fallback.
pub fn is_known_package(package: &str) -> bool {
    const KNOWN: [&str; 12] = [
        "anthropic",
        "openai",
        "openrouter",
        "google",
        "vertex",
        "azure",
        "bedrock",
        "xai",
        "ollama",
        "lmstudio",
        "vllm",
        "copilot",
    ];
    KNOWN.iter().any(|k| package.contains(k))
}

/// Strip a context-window hint suffix such as `[1m]` / `[200k]` / `[500k]`
/// that Claude Code appends to unknown gateway model ids. Returns the base id.
/// Any trailing `[<digits>k|m]` (case-insensitive) is stripped; anything else
/// (e.g. `[foo]`, `[12]`) is left untouched.
pub fn strip_window_suffix(s: &str) -> &str {
    if !s.ends_with(']') {
        return s;
    }
    let Some(open) = s.rfind('[') else {
        return s;
    };
    let inner = s[open + 1..s.len() - 1].to_lowercase();
    let (digits, unit) = inner.split_at(inner.len().saturating_sub(1));
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && matches!(unit, "k" | "m")
    {
        &s[..open]
    } else {
        s
    }
}
/// Sanitize a model id into a URL/claude-safe slug.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Build the automatic gateway alias for a catalog entry.
///
/// The `claude-` prefix is intentional: Claude Code gateway discovery only
/// keeps `/v1/models` entries whose id contains `claude` or `anthropic`.
pub fn auto_alias(entry: &CatalogEntry) -> AliasEntry {
    let slug = slugify(&format!("{}-{}", entry.provider_id, entry.model_id));
    let gateway_id = format!("claude-{slug}");
    AliasEntry {
        gateway_id,
        opencode_ref: entry.qualified(),
        display_name: format!("{} ({})", entry.name, entry.provider_id),
        description: format!("via frank-opencode · {}", entry.qualified()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_provider_model_with_slashes() {
        let r = ModelRef::parse("openrouter/anthropic/claude-sonnet-4-5").unwrap();
        assert_eq!(r.provider_id, "openrouter");
        assert_eq!(r.model_id, "anthropic/claude-sonnet-4-5");
    }

    #[test]
    fn strips_variant_suffix() {
        let r = ModelRef::parse("openai/gpt-5.2#high").unwrap();
        assert_eq!(r.model_id, "gpt-5.2");
    }

    #[test]
    fn rejects_invalid_refs() {
        assert!(ModelRef::parse("no-slash").is_none());
        assert!(ModelRef::parse("/empty-provider").is_none());
        assert!(ModelRef::parse("provider/").is_none());
    }

    #[test]
    fn auto_alias_contains_claude_prefix() {
        let e = CatalogEntry {
            id: "kimi-k2.7-code".into(),
            model_id: "kimi-k2.7-code".into(),
            provider_id: "opencode-go".into(),
            name: "Kimi K2.7 Code".into(),
            package: "@opencode/ai/providers/openai-compatible".into(),
            settings: CatalogSettings::default(),
            enabled: true,
        };
        let a = auto_alias(&e);
        assert!(a.gateway_id.contains("claude"));
        assert_eq!(a.opencode_ref, "opencode-go/kimi-k2.7-code");
    }

    #[test]
    fn slugify_collapses_separators() {
        assert_eq!(
            slugify("OpenRouter/Claude Sonnet 4.5"),
            "openrouter-claude-sonnet-4-5"
        );
    }

    #[test]
    fn protocol_table() {
        use Protocol::*;
        assert_eq!(protocol_for("@opencode/ai/providers/anthropic"), Anthropic);
        assert_eq!(
            protocol_for("@opencode/ai/providers/anthropic-compatible"),
            Anthropic
        );
        assert_eq!(protocol_for("@opencode/ai/providers/openai"), Responses);
        assert_eq!(
            protocol_for("@opencode/ai/providers/openai/responses"),
            Responses
        );
        assert_eq!(
            protocol_for("@opencode/ai/providers/openai-compatible"),
            ChatCompletions
        );
        assert_eq!(
            protocol_for("@opencode/ai/providers/openrouter"),
            ChatCompletions
        );
        assert!(is_known_package("@opencode/ai/providers/openrouter"));
        assert!(!is_known_package("@acme/custom-thing"));
        assert_eq!(protocol_for("@acme/custom-thing"), ChatCompletions);
    }

    #[test]
    fn strips_window_hint_suffix() {
        assert_eq!(strip_window_suffix("claude-x[1m]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[200K]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[500k]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x[2M]"), "claude-x");
        assert_eq!(strip_window_suffix("claude-x"), "claude-x");
        assert_eq!(strip_window_suffix("a/b[1m]"), "a/b");
        // Not a window hint: left untouched.
        assert_eq!(strip_window_suffix("claude-x[foo]"), "claude-x[foo]");
        assert_eq!(strip_window_suffix("claude-x[12]"), "claude-x[12]");
        assert_eq!(strip_window_suffix("claude-x[]"), "claude-x[]");
        assert_eq!(strip_window_suffix("claude-x[k]"), "claude-x[k]");
        assert_eq!(strip_window_suffix("claude-x"), "claude-x");
        assert_eq!(strip_window_suffix("no-bracket"), "no-bracket");
    }
}
