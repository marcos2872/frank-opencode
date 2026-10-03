//! Wire protocol routing for provider packages.

use crate::domain::model::CatalogEntry;

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

/// Wire protocol for a catalog entry, honoring the per-model
/// `settings.endpoint` the catalog declares.
///
/// `github-copilot` (aisdk package) mixes APIs on one baseURL: GPT-6/5.6,
/// grok, mai-code and codex run on the Responses API (`endpoint: "responses"`),
/// Gemini on Chat Completions (`"chat"`) and Claude on Messages (`"messages"`).
/// Routing by package alone sends the Responses models to `/chat/completions`,
/// which Copilot rejects with `400 ... not accessible via the /chat/completions
/// endpoint`. For aisdk entries the declared endpoint decides; all other
/// packages keep the `protocol_for(package)` table.
pub fn protocol_for_entry(entry: &CatalogEntry) -> Protocol {
    if entry.package.contains("aisdk") {
        return match entry.settings.endpoint.as_deref() {
            Some("responses") => Protocol::Responses,
            Some("messages") => Protocol::Anthropic,
            Some("chat") => Protocol::ChatCompletions,
            // No endpoint declared: historical ChatCompletions fallback, kept
            // quiet (some aisdk rows legitimately omit it).
            None => Protocol::ChatCompletions,
            // Unknown endpoint label: same fallback, but loud — a
            // miscataloged row silently hitting the wrong wire protocol is
            // a 400 factory.
            Some(other) => {
                tracing::warn!(
                    provider_id = %entry.provider_id,
                    package = %entry.package,
                    endpoint = other,
                    "unknown aisdk endpoint, using ChatCompletions fallback"
                );
                Protocol::ChatCompletions
            }
        };
    }
    protocol_for(&entry.package)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::CatalogSettings;
    use pretty_assertions::assert_eq;

    /// The github-copilot aisdk package mixes APIs under one baseURL; the
    /// catalog's per-model `settings.endpoint` decides the wire protocol.
    fn copilot_entry(endpoint: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            id: "m".into(),
            model_id: "m".into(),
            provider_id: "github-copilot".into(),
            name: "M".into(),
            package: "aisdk:@ai-sdk/github-copilot".into(),
            settings: CatalogSettings {
                endpoint: endpoint.map(|s| s.to_string()),
                ..CatalogSettings::default()
            },
            limit: None,
            enabled: true,
            variants: vec![],
            headers: None,
            body: None,
        }
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
    fn protocol_for_entry_honors_copilot_endpoint() {
        use Protocol::*;
        assert_eq!(
            protocol_for_entry(&copilot_entry(Some("responses"))),
            Responses
        );
        assert_eq!(
            protocol_for_entry(&copilot_entry(Some("chat"))),
            ChatCompletions
        );
        assert_eq!(
            protocol_for_entry(&copilot_entry(Some("messages"))),
            Anthropic
        );
        // Unknown/absent endpoint on aisdk: ChatCompletions fallback.
        assert_eq!(
            protocol_for_entry(&copilot_entry(Some("weird"))),
            ChatCompletions
        );
        assert_eq!(protocol_for_entry(&copilot_entry(None)), ChatCompletions);
    }

    #[test]
    fn protocol_for_entry_ignores_endpoint_outside_aisdk() {
        use Protocol::*;
        let mut e = CatalogEntry {
            id: "c".into(),
            model_id: "claude-sonnet-5.5".into(),
            provider_id: "github-copilot".into(),
            name: "Claude".into(),
            package: "@opencode/ai/providers/anthropic".into(),
            settings: CatalogSettings {
                endpoint: Some("chat".into()),
                ..CatalogSettings::default()
            },
            limit: None,
            enabled: true,
            variants: vec![],
            headers: None,
            body: None,
        };
        // Anthropic package wins regardless of a stray endpoint label.
        assert_eq!(protocol_for_entry(&e), Anthropic);
        // OpenAI responses-package stays Responses with no endpoint declared.
        e.package = "@opencode/ai/providers/openai".into();
        e.settings.endpoint = None;
        assert_eq!(protocol_for_entry(&e), Responses);
    }
}