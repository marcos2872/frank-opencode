//! Domain model: catalog rows, refs and variants. No I/O.

use serde::Deserialize;

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
    /// Per-model default headers from the catalog (e.g. `anthropic-beta:
    /// fast-mode-2026-02-01` for `*-fast` rows, `x-opencode-org-id` for Go
    /// inference rows). Forwarded on every upstream request.
    #[serde(default)]
    pub headers: Option<std::collections::HashMap<String, String>>,
    /// Per-model default body fields from the catalog (e.g.
    /// `{"speed":"fast"}`). Merged into the upstream JSON body.
    #[serde(default)]
    pub body: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogSettings {
    #[serde(rename = "baseURL", default)]
    pub base_url: Option<String>,
    #[serde(rename = "apiKey", default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Wire API for the model, per the catalog (`settings.endpoint`).
    /// `github-copilot` (aisdk package) mixes APIs: `"responses"` (GPT-6/5.6,
    /// grok, mai-code, codex), `"chat"` (Gemini), `"messages"` (Claude).
    #[serde(default)]
    pub endpoint: Option<String>,
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
    /// Messages-API effort label the catalog carries alongside `thinking`
    /// (e.g. `{"thinking": "high"}`); distinct from `reasoning_effort`.
    #[serde(default)]
    pub effort: Option<String>,
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

    /// Stable identity for this catalog row: `provider/id`.
    /// For most rows `id == modelID` so this equals `qualified()`; for rows
    /// like `claude-opus-4.8-fast` (same `modelID`, different `id`/`headers`)
    /// it is distinct and lets the gateway keep both flavors.
    pub fn id_ref(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }

    /// Reference to advertise for this row: `qualified()` normally,
    /// `id_ref()` when the row carries a distinct `id` (fast flavors etc.)
    /// so `lookup_entry` can resolve it via the `id` match.
    pub fn preferred_ref(&self) -> String {
        if !self.id.is_empty() && self.id != self.model_id {
            self.id_ref()
        } else {
            self.qualified()
        }
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

    fn test_entry(provider: &str, id: &str, model: &str, name: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.into(),
            model_id: model.into(),
            provider_id: provider.into(),
            name: name.into(),
            package: "@opencode/ai/providers/anthropic".into(),
            settings: CatalogSettings::default(),
            limit: None,
            enabled: true,
            variants: vec![],
            headers: None,
            body: None,
        }
    }

    #[test]
    fn preferred_ref_uses_id_when_distinct() {
        let normal = test_entry("github-copilot", "claude-opus-4.8", "claude-opus-4.8", "N");
        assert_eq!(normal.preferred_ref(), "github-copilot/claude-opus-4.8");
        assert_eq!(normal.id_ref(), "github-copilot/claude-opus-4.8");
        let fast = test_entry(
            "github-copilot",
            "claude-opus-4.8-fast",
            "claude-opus-4.8",
            "N Fast",
        );
        assert_eq!(fast.preferred_ref(), "github-copilot/claude-opus-4.8-fast");
    }

    #[test]
    fn catalog_headers_body_deserialize() {
        use serde_json::json;
        let e: CatalogEntry = serde_json::from_value(json!({
            "id": "claude-opus-4.8-fast",
            "modelID": "claude-opus-4.8",
            "providerID": "github-copilot",
            "name": "Claude Opus 4.8 Fast",
            "package": "@opencode/ai/providers/anthropic",
            "enabled": true,
            "headers": {"anthropic-beta": "fast-mode-2026-02-01"},
            "body": {"speed": "fast"}
        }))
        .unwrap();
        assert_eq!(
            e.headers.as_ref().unwrap()["anthropic-beta"],
            "fast-mode-2026-02-01"
        );
        assert_eq!(e.body.as_ref().unwrap()["speed"], "fast");
        assert_eq!(e.preferred_ref(), "github-copilot/claude-opus-4.8-fast");
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
}