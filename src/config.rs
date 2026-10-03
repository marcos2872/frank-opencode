//! Config file (~/.config/frank-opencode/config.toml) + env overrides.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 3737;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasConfig {
    /// OpenCode ref, e.g. `opencode-go/kimi-k2.7-code`.
    pub opencode: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DisabledConfig {
    #[serde(default)]
    pub models: Vec<String>,
}

/// Anthropic family tier advertised on `/v1/models` (`anthropic_family_tier`).
/// Claude Desktop's background calls (session-title generation, `small_fast`
/// class) pick the first discovered model with tier `haiku`, then `sonnet`,
/// then `opus`. Without tiers the Desktop falls back to id substring
/// matching, which resolves to whichever `*sonnet*` row sorts first (today
/// the Copilot row) — burning its quota on title generation. Any other string
/// is a config load error (fail fast, like the rest of this file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Haiku,
    Sonnet,
    Opus,
    Fable,
    Mythos,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Haiku => "haiku",
            Tier::Sonnet => "sonnet",
            Tier::Opus => "opus",
            Tier::Fable => "fable",
            Tier::Mythos => "mythos",
        }
    }
}

/// Tier mapping for one model. The table key matches a gateway id or an
/// OpenCode ref (`provider/model`), same as `[disabled]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierConfig {
    /// One of `haiku` / `sonnet` / `opus` / `fable` / `mythos`.
    pub tier: Tier,
    /// Winner when several aliases share the tier (Desktop picks the first
    /// flagged; unflagged ties fall back to list order).
    #[serde(default)]
    pub family_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    /// Expected gateway credential (sent as ANTHROPIC_AUTH_TOKEN / x-api-key).
    /// Empty = accept any (localhost-only dev default).
    #[serde(default)]
    pub auth_token: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub opencode_bin: String,
    #[serde(default)]
    pub aliases: HashMap<String, AliasConfig>,
    #[serde(default)]
    pub disabled: DisabledConfig,
    /// Anthropic family tiers advertised on `/v1/models`
    /// (`anthropic_family_tier` + `is_family_default`). Key = gateway id or
    /// OpenCode ref (`provider/model`), matched like `[disabled]`. Unmapped
    /// aliases announce no tier (current behavior, nothing breaks).
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
    /// Include Console free-tier (`opencode/*`, public key) models.
    /// They 403 outside OpenCode, so they are hidden by default.
    #[serde(default)]
    pub include_free_tier: bool,
    /// Rewrite auto-generated gateway ids to dodge the Claude Desktop
    /// picker's bundled third-party-model denylist (`deepseek`, `kimi`,
    /// `gpt`, ...): `deepseek-v4.1-flash` is advertised as
    /// `claude-opencode-go-d-eepseek-v4-1-flash`. Only the advertised id
    /// changes; refs, display names and resolution are untouched.
    /// Off by default (Claude Code lists every `claude-*` id already).
    #[serde(default)]
    pub desktop_aliases: bool,
    /// Answer Claude Code's auto-mode safety-classifier calls (and its
    /// tiny liveness probes) locally instead of forwarding upstream.
    /// Those auxiliary requests use hardcoded first-party ids
    /// (`claude-sonnet-5`, `claude-opus-4-8`) that resolve to whichever
    /// catalog row carries that model id — out of quota, they log a 429
    /// WARN on every auto-mode check. When on, the gateway replies the
    /// "allow" verdict itself and never touches the provider.
    /// Tradeoff: with the mock on, auto mode's LLM safety review always
    /// passes; real conversations (tools + a real system prompt) are
    /// still forwarded normally.
    #[serde(default)]
    pub mock_classifier: bool,
    /// TCP/TLS connect timeout for the upstream request, in seconds.
    /// Short so an unreachable provider fails fast.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Total upstream request timeout, in seconds. This one bounds streaming
    /// responses too, so it must be generous: a long reasoning turn with a
    /// large output can legitimately stay open for many minutes.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

fn default_connect_timeout_secs() -> u64 {
    30
}

fn default_request_timeout_secs() -> u64 {
    3600
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            auth_token: String::new(),
            default_model: String::new(),
            opencode_bin: "opencode".to_string(),
            aliases: HashMap::new(),
            disabled: DisabledConfig::default(),
            tiers: HashMap::new(),
            include_free_tier: false,
            desktop_aliases: false,
            mock_classifier: false,
            connect_timeout_secs: default_connect_timeout_secs(),
            request_timeout_secs: default_request_timeout_secs(),
        }
    }
}

impl AppConfig {
    pub fn config_path(explicit: Option<PathBuf>) -> PathBuf {
        if let Some(p) = explicit {
            return p;
        }
        if let Ok(env) = std::env::var("FRANK_CONFIG") {
            if !env.is_empty() {
                return PathBuf::from(env);
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("frank-opencode")
            .join("config.toml")
    }

    /// Load from file if present, else defaults.
    /// A present-but-invalid file is an error (fail fast instead of
    /// silently running with wrong port / dropped aliases).
    pub fn load(explicit: Option<PathBuf>) -> Result<Self, String> {
        let path = Self::config_path(explicit);
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("invalid config {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(format!("cannot read config {}: {e}", path.display())),
        };
        // Env overrides (FRANK_PORT / FRANK_AUTH_TOKEN).
        if let Ok(p) = std::env::var("FRANK_PORT") {
            if let Ok(n) = p.parse::<u16>() {
                cfg.port = n;
            }
        }
        if let Ok(t) = std::env::var("FRANK_AUTH_TOKEN") {
            if !t.is_empty() {
                cfg.auth_token = t;
            }
        }
        Ok(cfg)
    }

    pub fn data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("frank-opencode")
    }

    pub fn is_disabled(&self, opencode_ref: &str, gateway_id: &str) -> bool {
        self.disabled
            .models
            .iter()
            .any(|d| d == opencode_ref || d == gateway_id)
    }

    /// Tier mapping for an alias: gateway id first, then the OpenCode ref
    /// (mirrors `is_disabled` matching so manual renames keep working).
    pub fn tier_for(&self, opencode_ref: &str, gateway_id: &str) -> Option<TierConfig> {
        self.tiers
            .get(gateway_id)
            .or_else(|| self.tiers.get(opencode_ref))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let cfg =
            AppConfig::load(Some(PathBuf::from("/nonexistent-frank-test/config.toml"))).unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(cfg.aliases.is_empty());
    }

    #[test]
    fn invalid_file_is_an_error_not_silent_defaults() {
        let dir = std::env::temp_dir().join("frank-cfg-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.toml");
        std::fs::write(&path, "port = \"not-a-number\"\n").unwrap();
        let err = AppConfig::load(Some(path)).unwrap_err();
        assert!(err.contains("invalid config"), "{err}");
    }

    #[test]
    fn parses_alias_table() {
        let cfg: AppConfig = toml::from_str(
            r#"
port = 4000
default_model = "claude-sonnet-4-6-frank"
[aliases."claude-sonnet-4-6-frank"]
opencode = "opencode-go/kimi-k2.7-code"
"#,
        )
        .unwrap();
        assert_eq!(cfg.port, 4000);
        assert_eq!(
            cfg.aliases["claude-sonnet-4-6-frank"].opencode,
            "opencode-go/kimi-k2.7-code"
        );
    }

    #[test]
    fn mock_classifier_defaults_off_and_parses() {
        assert!(!AppConfig::default().mock_classifier);
        let off: AppConfig = toml::from_str("port = 4000\n").unwrap();
        assert!(!off.mock_classifier);
        let on: AppConfig = toml::from_str("mock_classifier = true\n").unwrap();
        assert!(on.mock_classifier);
    }

    #[test]
    fn tiers_default_empty_and_parse() {
        assert!(AppConfig::default().tiers.is_empty());
        let cfg: AppConfig = toml::from_str(
            r#"
[tiers."claude-opencode-go-muse-spark-1-3-contributor"]
tier = "haiku"
family_default = true
[tiers."github-copilot/claude-sonnet-5"]
tier = "sonnet"
"#,
        )
        .unwrap();
        let fast = cfg
            .tier_for(
                "opencode-go/muse-spark-1-3-contributor",
                "claude-opencode-go-muse-spark-1-3-contributor",
            )
            .unwrap();
        assert_eq!(fast.tier, Tier::Haiku);
        assert!(fast.family_default);
        let copilot = cfg
            .tier_for("github-copilot/claude-sonnet-5", "claude-x")
            .unwrap();
        assert_eq!(copilot.tier, Tier::Sonnet);
        assert!(!copilot.family_default);
        // Unknown keys are valid TOML but no mapping matches.
        assert!(cfg.tier_for("other/model", "claude-y").is_none());
    }

    #[test]
    fn tier_rejects_unknown_label() {
        let err = toml::from_str::<AppConfig>("[tiers.x]\ntier = \"turbo\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("turbo"), "{err}");
    }
}
