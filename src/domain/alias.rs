//! Gateway aliases advertised on `GET /v1/models`.

use crate::domain::desktop::evade_desktop_blocklist;
use crate::domain::model::CatalogEntry;
use crate::domain::slug::slugify;
use serde::{Deserialize, Serialize};

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

/// Build the automatic gateway alias for a catalog entry.
///
/// The `claude-` prefix is intentional: Claude Code gateway discovery only
/// keeps `/v1/models` entries whose id contains `claude` or `anthropic`.
pub fn auto_alias(entry: &CatalogEntry) -> AliasEntry {
    let slug = slugify(&format!("{}-{}", entry.provider_id, entry.model_id));
    let gateway_id = format!("claude-{slug}");
    let opencode_ref = entry.preferred_ref();
    AliasEntry {
        gateway_id,
        opencode_ref: opencode_ref.clone(),
        display_name: format!("{} ({})", entry.name, entry.provider_id),
        description: format!("via frank-opencode · {opencode_ref}"),
        context_window: entry.context_window(),
    }
}

/// Upper bound on numeric suffix attempts when disambiguating gateway ids.
/// Past this the gateway warns and keeps the last candidate instead of
/// looping forever (the old code broke out silently).
const MAX_ALIAS_SUFFIX: i32 = 100;

/// Candidate gateway ids for one catalog row, in preference order.
///
/// The base alias (`provider-model`) comes first and keeps the first row;
/// rows with a distinct `id` (fast flavors etc.) fall back to `provider-id`;
/// then numeric suffixes, alternating `id-based-n` / `base-n`.
///
/// Note: for 1- and 2-way collisions (everything real catalogs produce:
/// normal + fast flavor, or a punctuation pair like `v4.1` vs `v4-1`) this
/// yields byte-identical ids to the previous cascading logic. For 3+-way
/// collisions the old code picked an erratic (but still unique) order; this
/// picks the first free candidate deterministically instead.
fn alias_candidates(base_id: &str, id_based: &str) -> impl Iterator<Item = String> {
    let mut out = vec![base_id.to_string()];
    if id_based != base_id {
        out.push(id_based.to_string());
        for n in 2..=MAX_ALIAS_SUFFIX {
            out.push(format!("{id_based}-{n}"));
            out.push(format!("{base_id}-{n}"));
        }
    } else {
        for n in 2..=MAX_ALIAS_SUFFIX {
            out.push(format!("{base_id}-{n}"));
        }
    }
    out.into_iter()
}

/// Build automatic aliases for a batch, guaranteeing unique `gateway_id`s.
///
/// Two rows can slug to the same id: same `provider/modelID` with different
/// `id` (e.g. `claude-opus-4.8` vs `claude-opus-4.8-fast`), or different
/// model ids that differ only by punctuation (`v4.1` vs `v4-1`). The first
/// row (sorted by `qualified`, then `id`) keeps the base alias; the rest
/// take the first free candidate from [`alias_candidates`]. Callers must
/// still dedup against manual aliases (see `AppState::refresh`).
///
/// When `evade` is set (opt-in `desktop_aliases` config for Claude Desktop,
/// whose bundled denylist drops discovered ids containing third-party model
/// tokens), the model/id fragments are rewritten via
/// `evade_desktop_blocklist` before slugging. `opencode_ref`, display names
/// and resolution are unaffected: only the advertised `gateway_id` changes.
pub fn auto_aliases_for(entries: &[CatalogEntry], evade: bool) -> Vec<AliasEntry> {
    use std::collections::HashSet;
    let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.qualified()
            .cmp(&b.qualified())
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(sorted.len());
    for e in sorted {
        let model_part = if evade {
            evade_desktop_blocklist(&e.model_id)
        } else {
            e.model_id.clone()
        };
        let id_part = if !e.id.is_empty() {
            if evade {
                evade_desktop_blocklist(&e.id)
            } else {
                e.id.clone()
            }
        } else {
            model_part.clone()
        };
        let base_slug = slugify(&format!("{}-{}", e.provider_id, model_part));
        let base_id = format!("claude-{base_slug}");
        // Distinct-id rows advertise `provider/id` so `lookup_entry` can
        // resolve them via the `id` match instead of collapsing to the first
        // row with the same `modelID`.
        let opencode_ref = e.preferred_ref();
        let id_based = if id_part == model_part && e.id.is_empty() {
            base_id.clone()
        } else {
            format!(
                "claude-{}",
                slugify(&format!("{}-{}", e.provider_id, id_part))
            )
        };
        let mut chosen: Option<String> = None;
        for candidate in alias_candidates(&base_id, &id_based) {
            if !taken.contains(&candidate) {
                chosen = Some(candidate);
                break;
            }
        }
        let candidate = match chosen {
            Some(c) => c,
            None => {
                tracing::warn!(
                    opencode_ref = %opencode_ref,
                    "alias pool exhausted; reusing last candidate (duplicate gateway_id risk)"
                );
                format!("{base_id}-{MAX_ALIAS_SUFFIX}")
            }
        };
        taken.insert(candidate.clone());
        out.push(AliasEntry {
            gateway_id: candidate,
            opencode_ref: opencode_ref.clone(),
            display_name: format!("{} ({})", e.name, e.provider_id),
            description: format!("via frank-opencode · {opencode_ref}"),
            context_window: e.context_window(),
        });
    }
    out.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{CatalogEntry, CatalogSettings};
    use pretty_assertions::assert_eq;

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
            headers: None,
            body: None,
        };
        let a = auto_alias(&e);
        assert!(a.gateway_id.contains("claude"));
        assert_eq!(a.opencode_ref, "opencode-go/kimi-k2.7-code");
        assert_eq!(a.context_window, None);
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
            headers: None,
            body: None,
        };
        let a = auto_alias(&e);
        assert!(a.gateway_id.contains("claude"));
        assert_eq!(a.opencode_ref, "opencode-go/kimi-k2.7-code");
        assert_eq!(a.context_window, None);
    }

    #[test]
    fn auto_aliases_disambiguate_same_model_id() {
        let normal = test_entry(
            "github-copilot",
            "claude-opus-4.8",
            "claude-opus-4.8",
            "Claude Opus 4.8",
        );
        let mut fast = test_entry(
            "github-copilot",
            "claude-opus-4.8-fast",
            "claude-opus-4.8",
            "Claude Opus 4.8 Fast",
        );
        fast.headers = Some(
            [(
                "anthropic-beta".to_string(),
                "fast-mode-2026-02-01".to_string(),
            )]
            .into_iter()
            .collect(),
        );
        fast.body = Some(serde_json::json!({"speed": "fast"}));
        let aliases = auto_aliases_for(&[normal, fast], false);
        assert_eq!(aliases.len(), 2);
        let ids: Vec<&str> = aliases.iter().map(|a| a.gateway_id.as_str()).collect();
        // No duplicates.
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 2, "{ids:?}");
        // Base alias kept for the normal row, fast gets an id-based alias.
        assert!(ids.contains(&"claude-github-copilot-claude-opus-4-8"));
        assert!(ids.contains(&"claude-github-copilot-claude-opus-4-8-fast"));
        let refs: Vec<&str> = aliases.iter().map(|a| a.opencode_ref.as_str()).collect();
        assert!(refs.contains(&"github-copilot/claude-opus-4.8"));
        assert!(refs.contains(&"github-copilot/claude-opus-4.8-fast"));
    }

    #[test]
    fn auto_aliases_dedup_slug_collision() {
        // `v4.1` vs `v4-1` slug to the same id; both must survive with
        // distinct gateway ids.
        let a = test_entry(
            "opencode-go",
            "deepseek-v4.1-flash",
            "deepseek-v4.1-flash",
            "A",
        );
        let b = test_entry(
            "opencode-go",
            "deepseek-v4-1-flash",
            "deepseek-v4-1-flash",
            "B",
        );
        let aliases = auto_aliases_for(&[a, b], false);
        assert_eq!(aliases.len(), 2);
        assert_ne!(aliases[0].gateway_id, aliases[1].gateway_id);
    }

    /// Mirror of the Desktop picker's gateway-id rule (bundled `Lo`: id must
    /// contain `claude` and none of the `XSe` denylist tokens) over the token
    /// subset that can occur in our slugs.
    fn desktop_would_list(id: &str) -> bool {        let lower = id.to_lowercase();
        let has_claude = lower.contains("claude");
        let blocked = [
            "deepseek", "gemini", "glm", "gpt", "grok", "hy3", "kimi", "qwen", "minimax",
            "longcat", "mimo",
        ];
        has_claude && !blocked.iter().any(|t| lower.contains(t))
    }

    #[test]
    fn evaded_aliases_pass_desktop_filter() {
        let models = [
            ("deepseek-v4.1-flash", "DeepSeek V4.1 Flash"),
            ("deepseek-v4-flash", "DeepSeek V4 Flash"),
            ("kimi-k2.7-code", "Kimi K2.7 Code"),
            ("glm-5.3", "GLM 5.3"),
            ("gpt-6-luna", "GPT-6 Luna"),
            ("grok-4.7", "Grok 4.7"),
            ("qwen3.8-max", "Qwen3.8 Max"),
            ("minimax-m3", "MiniMax M3"),
            ("longcat-2.0", "LongCat 2.0"),
            ("mimo-v2.5-pro", "MiMo-V2.5-Pro"),
            ("hy3", "HY3"),
            ("muse-spark-1.3-contributor", "Muse Spark 1.3"),
        ];
        let entries: Vec<CatalogEntry> = models
            .iter()
            .map(|(m, n)| test_entry("opencode-go", m, m, n))
            .collect();
        // Without evasion the Desktop drops all but the last one.
        let plain = auto_aliases_for(&entries, false);
        assert_eq!(
            plain
                .iter()
                .filter(|a| desktop_would_list(&a.gateway_id))
                .count(),
            1
        );
        // With evasion every alias passes, stays unique and keeps the
        // original ref for resolution.
        let evaded = auto_aliases_for(&entries, true);
        assert_eq!(evaded.len(), models.len());
        for a in &evaded {
            assert!(desktop_would_list(&a.gateway_id), "{}", a.gateway_id);
        }
        let mut ids: Vec<&str> = evaded.iter().map(|a| a.gateway_id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), models.len());
        let deepseek = evaded
            .iter()
            .find(|a| a.opencode_ref == "opencode-go/deepseek-v4.1-flash")
            .unwrap();
        assert!(deepseek.display_name.contains("DeepSeek"));
    }
}
