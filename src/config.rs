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
    /// Include Console free-tier (`opencode/*`, public key) models.
    /// They 403 outside OpenCode, so they are hidden by default.
    #[serde(default)]
    pub include_free_tier: bool,
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
            include_free_tier: false,
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
}
