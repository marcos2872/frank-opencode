//! Domain: pure types and rules. No tokio, axum, rusqlite here.

pub mod error;

pub use error::GatewayError;
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
/// Blocklist tokens longest-first, cached process-wide: `evade_*` is called
/// once per alias at catalog refresh, and no longer needs to copy + sort
/// the table on every call.
fn desktop_tokens_by_length() -> &'static [&'static str] {
    use std::sync::OnceLock;
    static SORTED: OnceLock<Vec<&'static str>> = OnceLock::new();
    SORTED.get_or_init(|| {
        let mut tokens: Vec<&'static str> = DESKTOP_BLOCKED_TOKENS.to_vec();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        tokens
    })
}

pub fn evade_desktop_blocklist(s: &str) -> String {
    let mut out = s.to_string();
    for &token in desktop_tokens_by_length() {
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
    let opencode_ref = entry.preferred_ref();
    AliasEntry {
        gateway_id,
        opencode_ref: opencode_ref.clone(),
        display_name: format!("{} ({})", entry.name, entry.provider_id),
        description: format!("via frank-opencode · {opencode_ref}"),
        context_window: entry.context_window(),
    }
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
        });
    }
    out.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    out
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
            headers: None,
            body: None,
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
}
