//! Model references: `provider/model` (+ optional `#variant`).

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
}
