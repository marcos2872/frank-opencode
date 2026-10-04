//! Gateway aliases exposed on `GET /v1/models`, including the automatic
//! id rewrites (Desktop denylist evasion, CLI family shield).

use super::catalog::CatalogEntry;
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
    /// Anthropic family tier advertised as `anthropic_family_tier`
    /// (`haiku`/`sonnet`/`opus`/`fable`/`mythos`), from `[tiers]` config.
    /// `None` announces no tier (Desktop falls back to id substring match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_tier: Option<String>,
    /// Winner when several aliases share the tier, advertised as
    /// `is_family_default` (the Desktop only honors it together with a tier).
    #[serde(default, skip_serializing_if = "is_false")]
    pub family_default: bool,
}

fn is_false(b: &bool) -> bool {
    !b
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
    let inner = &s[open + 1..s.len() - 1];
    if inner.is_empty() || !inner.is_ascii() {
        return s;
    }
    let inner = inner.to_ascii_lowercase();
    let (digits, unit) = inner.split_at(inner.len() - 1);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && matches!(unit, "k" | "m")
    {
        &s[..open]
    } else {
        s
    }
}
/// The `[1m]` suffix for a context window announced on `/v1/models`.
/// Mainline Claude Code only reads a window from the literal `[1m]` suffix
/// (regex `/\[1m\]/i`), never from arbitrary `[<digits>k|m]` — so a window
/// below 1M yields no suffix (`None`) and a window at/above 1M announces
/// `[1m]` (rounded down, never claiming more than the real window).
pub fn window_suffix(tokens: u64) -> Option<String> {
    (tokens >= 1_000_000).then(|| "[1m]".to_string())
}
/// Substrings the Claude Desktop app refuses anywhere in a gateway model
/// id: any discovered id containing one is dropped from the picker, even
/// with `claude` in it. Extracted from the denylist baked into the Desktop
/// bundle (`XSe`): only plain slug-compatible tokens are listed here
/// (dotted/bounded fragments like `k2\.` or `\bling\b` can never occur in
/// our slugs, which use `-` separators and no dots).
const DESKTOP_BLOCKED_TOKENS: &[&str] = &[
    "ark-code",
    "astron",
    "command-r",
    "deepseek",
    "doubao",
    "gemini",
    "gemma",
    "glm",
    "gpt",
    "grok",
    "hermes",
    "hy3",
    "kimi",
    "lfm",
    "llama",
    "longcat",
    "mimo",
    "minimax",
    "mistral",
    "mixtral",
    "moonshot",
    "nemotron",
    "openai",
    "qianfan",
    "qwen",
    "trinity",
    "abab",
    "jamba",
    "arctic",
    "solar",
    "mercury",
    "zamba",
    "ernie",
    "arcee",
    "nova-",
    "phi-",
    "tc-code",
    "kat-coder",
    "yi-",
    "devstral",
    "ministral",
    "stepfun",
    "bytedance",
    "hunyuan",
    "granite",
    "codex",
    "step-3",
    "seed-",
];

/// Rewrite a model/id fragment so no `DESKTOP_BLOCKED_TOKENS` entry survives
/// as a substring: a `-` is inserted after the first character of each
/// (case-insensitive) occurrence (`deepseek` → `d-eepseek`). The Desktop
/// filter only inspects the alias `id`, so display names and `opencode_ref`
/// resolution are untouched. Deterministic and slug-safe.
pub fn evade_desktop_blocklist(s: &str) -> String {
    let mut out = s.to_string();
    let mut tokens: Vec<&str> = DESKTOP_BLOCKED_TOKENS.to_vec();
    tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
    for token in tokens {
        let mut search_from = 0;
        loop {
            let lower = out.to_lowercase();
            let Some(rel) = lower[search_from..].find(token) else {
                break;
            };
            let at = search_from + rel;
            // Split "deepseek" into "d-eepseek". Byte-safe: tokens are ASCII.
            out.insert(at + 1, '-');
            search_from = at + token.len() + 1;
        }
    }
    out
}
/// Rewrite a model/id fragment so the Claude Code CLI no longer resolves it
/// as a first-party family id for background calls (`small_fast`, family
/// fallbacks). The CLI canonicalizes discovered ids by substring
/// (`claude-sonnet-*` / `claude-opus-*`), so any catalog row carrying that
/// spelling — today the `github-copilot` Claude rows, tomorrow any provider
/// that ships `claude-*` models — attracts side queries (title-gen etc.).
/// Mapping (`claude-sonnet` → `cs`, `claude-opus` → `co`, `claude-haiku` →
/// `ch`, `claude-fable` → `cf`, `claude-mythos` → `cm`, bare `claude` → `c`,
/// plus bare `sonnet`/`opus`/`haiku`/`fable`/`mythos` → `s`/`o`/`h`/`f`/`m`
/// for prefix-less future rows) breaks the match while keeping the id
/// readable. Only the advertised `gateway_id` changes: `opencode_ref`,
/// display names and resolution are untouched. Deterministic, slug-safe,
/// and disjoint from `evade_desktop_blocklist` (no shared tokens), so both
/// rewrites compose.
pub fn shield_cli_family_match(s: &str) -> String {
    let mut out = s.to_lowercase();
    for (from, to) in [
        ("claude-sonnet", "cs"),
        ("claude-opus", "co"),
        ("claude-haiku", "ch"),
        ("claude-fable", "cf"),
        ("claude-mythos", "cm"),
        ("claude", "c"),
        ("sonnet", "s"),
        ("opus", "o"),
        ("haiku", "h"),
        ("fable", "f"),
        ("mythos", "m"),
    ] {
        out = out.replace(from, to);
    }
    out
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
/// Options controlling the automatic gateway-id rewrites in
/// [`auto_aliases_for`]. A struct (not positional `bool`s) so call sites read
/// as `AliasOptions { evade: true, ..Default::default() }` and a third rewrite
/// later doesn't become a third flag.
#[derive(Debug, Clone, Copy, Default)]
pub struct AliasOptions {
    /// Opt-in `desktop_aliases`: rewrite `DESKTOP_BLOCKED_TOKENS` fragments
    /// via `evade_desktop_blocklist` before slugging.
    pub evade: bool,
    /// Default-on `cli_shield_aliases`: rewrite family spelling via
    /// `shield_cli_family_match` first. Disjoint from `evade`; both compose.
    pub shield: bool,
}

/// Build automatic aliases for a batch, guaranteeing unique `gateway_id`s.
///
/// Two rows can slug to the same id: same `provider/modelID` with different
/// `id` (e.g. `claude-opus-4.8` vs `claude-opus-4.8-fast`), or different
/// model ids that differ only by punctuation (`v4.1` vs `v4-1`). The first
/// row (sorted by `qualified`, then `id`) keeps the base alias; the rest
/// fall back to `provider-id` and then numeric suffixes. Callers must still
/// dedup against manual aliases (see `AppState::refresh`).
///
/// When `opts.evade` is set (opt-in `desktop_aliases` config for Claude
/// Desktop, whose bundled denylist drops discovered ids containing
/// third-party model tokens), the model/id fragments are rewritten via
/// `evade_desktop_blocklist` before slugging. When `opts.shield` is set
/// (default-on `cli_shield_aliases` config for the Claude Code CLI, which
/// resolves background models by id-substring family spelling), the fragments
/// are rewritten via `shield_cli_family_match` first. The rewrites are
/// disjoint and compose. `opencode_ref`, display names and resolution are
/// unaffected: only the advertised `gateway_id` changes.
pub fn auto_aliases_for(entries: &[CatalogEntry], opts: AliasOptions) -> Vec<AliasEntry> {
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
        // Shield first (family spelling), then evade (blocklist tokens):
        // disjoint rewrites that compose (`co-4-8` never contains a blocked
        // token, and `d-eepseek` never contains a family spelling).
        let shielded_model = if opts.shield {
            shield_cli_family_match(&e.model_id)
        } else {
            e.model_id.clone()
        };
        let model_part = if opts.evade {
            evade_desktop_blocklist(&shielded_model)
        } else {
            shielded_model
        };
        let shielded_id = if !e.id.is_empty() {
            if opts.shield {
                shield_cli_family_match(&e.id)
            } else {
                e.id.clone()
            }
        } else {
            String::new()
        };
        let id_part = if !shielded_id.is_empty() {
            if opts.evade {
                evade_desktop_blocklist(&shielded_id)
            } else {
                shielded_id
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
        let id_slug = if id_part == model_part && e.id.is_empty() {
            base_slug.clone()
        } else {
            slugify(&format!("{}-{}", e.provider_id, id_part))
        };
        let id_based = format!("claude-{id_slug}");
        // Candidate order: base, id-based, base-2, base-3, ...
        let mut candidate = base_id.clone();
        if taken.contains(&candidate) {
            candidate = id_based.clone();
        }
        let mut n = 2;
        while taken.contains(&candidate) {
            // If even the id-based form collides (identical rows or
            // punctuation-only differences), append a numeric suffix.
            if candidate == id_based {
                candidate = format!("{base_id}-{n}");
            } else {
                candidate = format!("{id_based}-{n}");
                if taken.contains(&candidate) {
                    candidate = format!("{base_id}-{n}");
                }
            }
            n += 1;
            if n > 100 {
                break;
            }
        }
        taken.insert(candidate.clone());
        out.push(AliasEntry {
            gateway_id: candidate,
            opencode_ref: opencode_ref.clone(),
            display_name: format!("{} ({})", e.name, e.provider_id),
            description: format!("via frank-opencode · {opencode_ref}"),
            context_window: e.context_window(),
            family_tier: None,
            family_default: false,
        });
    }
    out.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::catalog::CatalogSettings;
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
        let aliases = auto_aliases_for(&[e], AliasOptions::default());
        assert_eq!(aliases.len(), 1);
        let a = &aliases[0];
        assert!(a.gateway_id.contains("claude"));
        assert_eq!(a.opencode_ref, "opencode-go/kimi-k2.7-code");
        assert_eq!(a.context_window, None);
    }

    #[test]
    fn alias_serialization_omits_unset_fields() {
        let a = AliasEntry {
            gateway_id: "claude-x".into(),
            opencode_ref: "p/x".into(),
            display_name: "X".into(),
            description: "d".into(),
            context_window: None,
            family_tier: None,
            family_default: false,
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
    fn window_suffix_with_unicode_is_unchanged() {
        let input = "claude-x[é]";
        assert_eq!(strip_window_suffix(input), input);
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

    #[test]
    fn window_suffix_only_1m() {
        // Mainline Claude Code reads a window only from the literal `[1m]`
        // suffix; sub-1M windows announce nothing.
        assert_eq!(window_suffix(1_000_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(1_050_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(1_999_999), Some("[1m]".to_string()));
        assert_eq!(window_suffix(2_000_000), Some("[1m]".to_string()));
        assert_eq!(window_suffix(200_000), None);
        assert_eq!(window_suffix(128_000), None);
        assert_eq!(window_suffix(105_000), None);
        assert_eq!(window_suffix(0), None);
        // Round trip: what we announce is what the gateway strips again.
        assert_eq!(strip_window_suffix("claude-x[1m]"), "claude-x");
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
        let aliases = auto_aliases_for(&[normal, fast], AliasOptions::default());
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
        let aliases = auto_aliases_for(&[a, b], AliasOptions::default());
        assert_eq!(aliases.len(), 2);
        assert_ne!(aliases[0].gateway_id, aliases[1].gateway_id);
    }

    #[test]
    fn evade_breaks_blocked_substrings() {
        assert_eq!(
            evade_desktop_blocklist("deepseek-v4.1-flash"),
            "d-eepseek-v4.1-flash"
        );
        assert_eq!(evade_desktop_blocklist("kimi-k2.7-code"), "k-imi-k2.7-code");
        assert_eq!(evade_desktop_blocklist("qwen3.8-max"), "q-wen3.8-max");
        assert_eq!(evade_desktop_blocklist("hy3"), "h-y3");
        // Unblocked names pass through untouched.
        assert_eq!(
            evade_desktop_blocklist("muse-spark-1.3-contributor"),
            "muse-spark-1.3-contributor"
        );
        assert_eq!(
            evade_desktop_blocklist("space-bunny-free"),
            "space-bunny-free"
        );
    }

    /// Mirror of the Desktop picker's gateway-id rule (bundled `Lo`: id must
    /// contain `claude` and none of the `XSe` denylist tokens) over the token
    /// subset that can occur in our slugs.
    fn desktop_would_list(id: &str) -> bool {
        let lower = id.to_lowercase();
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
        let plain = auto_aliases_for(&entries, AliasOptions::default());
        assert_eq!(
            plain
                .iter()
                .filter(|a| desktop_would_list(&a.gateway_id))
                .count(),
            1
        );
        // With evasion every alias passes, stays unique and keeps the
        // original ref for resolution.
        let evaded = auto_aliases_for(
            &entries,
            AliasOptions {
                evade: true,
                ..AliasOptions::default()
            },
        );
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

    #[test]
    fn shield_rewrites_family_spelling() {
        assert_eq!(shield_cli_family_match("claude-sonnet-5"), "cs-5");
        assert_eq!(shield_cli_family_match("claude-sonnet-5.5"), "cs-5.5");
        assert_eq!(shield_cli_family_match("claude-opus-4.8"), "co-4.8");
        assert_eq!(
            shield_cli_family_match("claude-opus-4.8-fast"),
            "co-4.8-fast"
        );
        assert_eq!(shield_cli_family_match("claude-haiku-4.5"), "ch-4.5");
        assert_eq!(shield_cli_family_match("claude-fable-1"), "cf-1");
        assert_eq!(shield_cli_family_match("claude-mythos-2"), "cm-2");
        // Non-family names pass through untouched.
        assert_eq!(
            shield_cli_family_match("muse-spark-1.3-contributor"),
            "muse-spark-1.3-contributor"
        );
        assert_eq!(shield_cli_family_match("kimi-k2.7-code"), "kimi-k2.7-code");
        assert_eq!(shield_cli_family_match("gpt-6-luna"), "gpt-6-luna");
    }

    /// Mirror of the CLI's background-model rule: it canonicalizes discovered
    /// ids by family substring (`claude-sonnet-*` / `claude-opus-*`, plus
    /// haiku/fable/mythos spellings for future rows) and ignores
    /// `anthropic_family_tier`.
    fn cli_would_match_family(id: &str) -> bool {
        let lower = id.to_lowercase();
        [
            "claude-sonnet",
            "claude-opus",
            "claude-haiku",
            "claude-fable",
            "claude-mythos",
        ]
        .iter()
        .any(|t| lower.contains(t))
    }

    #[test]
    fn shielded_aliases_break_cli_family_match() {
        let models = [
            (
                "github-copilot",
                "claude-sonnet-5",
                "claude-sonnet-5",
                "Claude Sonnet 5",
            ),
            (
                "github-copilot",
                "claude-sonnet-5.5",
                "claude-sonnet-5.5",
                "Claude Sonnet 5.5",
            ),
            (
                "github-copilot",
                "claude-opus-4.8",
                "claude-opus-4.8",
                "Claude Opus 4.8",
            ),
            (
                "github-copilot",
                "claude-opus-4.8-fast",
                "claude-opus-4.8",
                "Claude Opus 4.8 Fast",
            ),
            (
                "github-copilot",
                "claude-opus-5",
                "claude-opus-5",
                "Claude Opus 5",
            ),
            (
                "github-copilot",
                "claude-haiku-4.5",
                "claude-haiku-4.5",
                "Claude Haiku 4.5",
            ),
            (
                "some-provider",
                "claude-fable-1",
                "claude-fable-1",
                "Claude Fable 1",
            ),
            (
                "some-provider",
                "claude-mythos-2",
                "claude-mythos-2",
                "Claude Mythos 2",
            ),
            (
                "opencode-go",
                "kimi-k2.7-code",
                "kimi-k2.7-code",
                "Kimi K2.7 Code",
            ),
        ];
        let entries: Vec<CatalogEntry> = models
            .iter()
            .map(|(p, i, m, n)| test_entry(p, i, m, n))
            .collect();
        // Without the shield every family row matches the CLI's rule.
        let plain = auto_aliases_for(&entries, AliasOptions::default());
        assert_eq!(
            plain
                .iter()
                .filter(|a| cli_would_match_family(&a.gateway_id))
                .count(),
            8
        );
        // With the shield no advertised id matches, every row keeps a unique
        // id, and refs/display names are untouched for resolution and picker.
        let shielded = auto_aliases_for(
            &entries,
            AliasOptions {
                shield: true,
                ..AliasOptions::default()
            },
        );
        assert_eq!(shielded.len(), models.len());
        for a in &shielded {
            assert!(!cli_would_match_family(&a.gateway_id), "{}", a.gateway_id);
            // Discovery still requires the `claude` marker.
            assert!(a.gateway_id.contains("claude"), "{}", a.gateway_id);
        }
        let mut ids: Vec<&str> = shielded.iter().map(|a| a.gateway_id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), models.len());
        // Same `cs-`/`co-` spelling the manual aliases used.
        let by_ref = |r: &str| shielded.iter().find(|a| a.opencode_ref == r).unwrap();
        assert_eq!(
            by_ref("github-copilot/claude-sonnet-5").gateway_id,
            "claude-github-copilot-cs-5"
        );
        assert_eq!(
            by_ref("github-copilot/claude-opus-4.8-fast").gateway_id,
            "claude-github-copilot-co-4-8-fast"
        );
        assert_eq!(
            by_ref("github-copilot/claude-opus-4.8-fast").display_name,
            "Claude Opus 4.8 Fast (github-copilot)"
        );
        // Fast flavors still disambiguate under the shield (same modelID,
        // distinct id).
        assert_ne!(
            by_ref("github-copilot/claude-opus-4.8").gateway_id,
            by_ref("github-copilot/claude-opus-4.8-fast").gateway_id
        );
    }

    #[test]
    fn shield_composes_with_desktop_evade() {
        let entries = vec![
            test_entry(
                "github-copilot",
                "claude-sonnet-5",
                "claude-sonnet-5",
                "Sonnet",
            ),
            test_entry(
                "opencode-go",
                "deepseek-v4.1-flash",
                "deepseek-v4.1-flash",
                "DeepSeek",
            ),
        ];
        let both = auto_aliases_for(
            &entries,
            AliasOptions {
                evade: true,
                shield: true,
            },
        );
        assert_eq!(both.len(), 2);
        let by_ref = |r: &str| both.iter().find(|a| a.opencode_ref == r).unwrap();
        let copilot = by_ref("github-copilot/claude-sonnet-5");
        assert!(
            !cli_would_match_family(&copilot.gateway_id),
            "{}",
            copilot.gateway_id
        );
        let deepseek = by_ref("opencode-go/deepseek-v4.1-flash");
        assert!(
            desktop_would_list(&deepseek.gateway_id),
            "{}",
            deepseek.gateway_id
        );
        assert_ne!(copilot.gateway_id, deepseek.gateway_id);
    }
}
