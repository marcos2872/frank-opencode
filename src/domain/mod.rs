//! Domain: pure types and rules. No tokio, axum, rusqlite here.

use serde::{Deserialize, Serialize};

/// Reference to a model inside OpenCode: `provider/model-id`, optionally
/// `provider/model-id#variant` when the caller selects a model variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
    /// Selected variant label (e.g. `high`), when the caller asked for one.
    pub variant: Option<String>,
}

impl ModelRef {
    /// Parse `provider/model`, where model may itself contain `/`.
    /// A trailing `#variant` (everything from the last `#` on) is preserved
    /// as `variant`; the base model id is resolved as before.
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, model) = s.split_once('/')?;
        if provider.is_empty() || model.is_empty() || provider.contains('#') {
            return None;
        }
        let (model, variant) = match model.rfind('#') {
            Some(i) => (&model[..i], Some(model[i + 1..].to_string())),
            None => (model, None),
        };
        let variant = variant.filter(|v| !v.is_empty());
        if model.is_empty() {
            return None;
        }
        Some(Self {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
            variant,
        })
    }

    #[allow(dead_code)]
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    /// `provider/model#variant`, or just `provider/model` when unset.
    #[allow(dead_code)]
    pub fn fully_qualified(&self) -> String {
        match &self.variant {
            Some(v) => format!("{}/{}#{}", self.provider_id, self.model_id, v),
            None => self.qualified(),
        }
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
    pub limit: Option<CatalogLimit>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub variants: Vec<ModelVariant>,
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

/// Token limits as reported by `opencode api get /api/model` (`limit` field).
/// `context` is the full context window in tokens; `input`/`output` are
/// narrower budgets. The gateway only announces `context`.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogLimit {
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub input: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub output: Option<u64>,
}

/// One named variant of a model, as reported by `opencode api get /api/model`,
/// e.g. `{"id": "high", "settings": {"reasoningEffort": "high"}}`.
/// Settings keys vary by provider package (reasoningEffort, thinking,
/// budgetTokens, ...); we only deserialize what the forwards need.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelVariant {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub settings: ModelVariantSettings,
}

/// Thinking configuration as reported by the catalog.
///
/// v2 sends an object (`{"type":"disabled"}`, `{"type":"adaptive",
/// "display":"summarized"}`, ...); older catalogs used a plain label string.
/// Accept both, plus any future object shape, so an unexpected `thinking`
/// value can never break catalog parsing again.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ThinkingConfig {
    Label(String),
    Typed {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        display: Option<String>,
    },
    Other(serde_json::Map<String, serde_json::Value>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelVariantSettings {
    #[serde(rename = "reasoningEffort", default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(rename = "budgetTokens", default)]
    pub budget_tokens: Option<u64>,
    // In the v2 catalog this is a list of content parts to include in the
    // response (e.g. `["reasoning.encrypted_content"]`), not a boolean flag.
    #[serde(default)]
    pub include: Option<Vec<String>>,
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

    /// Context window in tokens as declared by the OpenCode catalog
    /// (`limit.context`), if the catalog exposes it.
    pub fn context_window(&self) -> Option<u64> {
        self.limit.as_ref().and_then(|l| l.context)
    }

    /// The declared variant with the given label, if any.
    pub fn variant(&self, label: &str) -> Option<&ModelVariant> {
        self.variants.iter().find(|v| v.id == label)
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
    /// Context window announced on `/v1/models` (tokens), from the catalog's
    /// `limit.context`. `None` when the catalog does not expose it (the field
    /// is then omitted from the JSON, so clients fall back to their default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
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
        context_window: entry.context_window(),
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
        assert_eq!(r.variant.as_deref(), Some("high"));
    }

    #[test]
    fn preserves_variant_with_slashes_in_model() {
        let r = ModelRef::parse("openrouter/anthropic/claude-sonnet-4-5#low").unwrap();
        assert_eq!(r.model_id, "anthropic/claude-sonnet-4-5");
        assert_eq!(r.variant.as_deref(), Some("low"));
    }

    #[test]
    fn no_variant_when_hash_absent_or_empty() {
        let r = ModelRef::parse("openai/gpt-5.2").unwrap();
        assert_eq!(r.variant, None);
        let r = ModelRef::parse("openai/gpt-5.2#").unwrap();
        assert_eq!(r.variant, None);
    }

    #[test]
    fn rejects_invalid_refs() {
        assert!(ModelRef::parse("no-slash").is_none());
        assert!(ModelRef::parse("/empty-provider").is_none());
        assert!(ModelRef::parse("provider/").is_none());
        assert!(ModelRef::parse("p#bad/m").is_none());
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
            limit: None,
            enabled: true,
            variants: vec![],
        };
        let a = auto_alias(&e);
        assert!(a.gateway_id.contains("claude"));
        assert_eq!(a.opencode_ref, "opencode-go/kimi-k2.7-code");
        assert_eq!(a.context_window, None);
    }

    #[test]
    fn context_window_from_catalog_limit() {
        use serde_json::json;
        let e: CatalogEntry = serde_json::from_value(json!({
            "modelID": "gemini-3.5-flash",
            "providerID": "github-copilot",
            "name": "Gemini 3.5 Flash",
            "package": "@opencode/ai/providers/openai-compatible",
            "enabled": true,
            "variants": [],
            "limit": {"context": 1000000, "input": 936000, "output": 64000}
        }))
        .unwrap();
        assert_eq!(e.context_window(), Some(1_000_000));
        // Catalog without a `limit` field keeps the window unset.
        let e2: CatalogEntry = serde_json::from_value(json!({
            "modelID": "x",
            "providerID": "p",
            "package": "anthropic",
            "enabled": true
        }))
        .unwrap();
        assert_eq!(e2.context_window(), None);
        // Alias serialization omits the field when unset and includes it when set.
        let a = AliasEntry {
            gateway_id: "claude-x".into(),
            opencode_ref: "p/x".into(),
            display_name: "X".into(),
            description: "d".into(),
            context_window: None,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"gateway_id":"claude-x","opencode_ref":"p/x","display_name":"X","description":"d"}"#
        );
        let a2 = AliasEntry {
            context_window: Some(1_000_000),
            ..a
        };
        let s = serde_json::to_string(&a2).unwrap();
        assert!(s.contains(r#""context_window":1000000"#), "{s}");
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
