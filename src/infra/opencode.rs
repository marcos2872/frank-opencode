//! Infra: how frank-opencode reads OpenCode v2 state.
//!
//! Rules (OpenCode v2):
//! - Credentials live in SQLite `opencode.db`, table `credential`.
//!   Path via `opencode debug paths db`, else OPENCODE_DB / XDG_DATA_HOME.
//! - `auth.json` is legacy migration input only, NOT the source of truth.
//! - Model catalog via `opencode api get /api/model` (handles service auth).

use crate::domain::CatalogEntry;
use secrecy::{ExposeSecret, SecretString};
use std::path::PathBuf;
use std::process::Command;

/// Resolve the OpenCode v2 database path without guessing.
pub fn resolve_db_path(opencode_bin: &str) -> PathBuf {
    // 1. Explicit env (documented override).
    if let Ok(p) = std::env::var("OPENCODE_DB") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // 2. Ask the binary itself (channel-aware).
    if let Ok(out) = Command::new(opencode_bin)
        .args(["debug", "paths", "db"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    // 3. Documented default: ~/.local/share/opencode/opencode.db (XDG aware).
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("opencode").join("opencode.db");
        }
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("opencode")
        .join("opencode.db")
}

/// Read-only credential access. `value` is only loaded in memory, never logged.
pub struct CredentialStore {
    pub db_path: PathBuf,
}

impl CredentialStore {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    /// Fetch the stored key for an integration id (e.g. `opencode-go`).
    /// Returns None when there is no active stored account.
    ///
    /// v2 stores a JSON envelope `{"type":..,"key":".."}` in `value`,
    /// so unwrap it; fall back to the raw value for forward compatibility.
    pub fn get(&self, integration_id: &str) -> Option<SecretString> {
        let conn = rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT value FROM credential WHERE integration_id = ?1 AND (active = 1 OR active IS NULL) LIMIT 1",
            )
            .ok()?;
        let value: Option<String> = stmt.query_row([integration_id], |row| row.get(0)).ok();
        value.map(|raw| SecretString::from(unwrap_envelope(&raw)))
    }

    /// List integrations with stored credentials (metadata only, no secrets).
    #[allow(dead_code)]
    pub fn list_integrations(&self) -> Vec<String> {
        let conn = match rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let mut stmt = match conn.prepare(
            "SELECT DISTINCT integration_id FROM credential WHERE active = 1 OR active IS NULL",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        rows.map(|r| r.filter_map(|x| x.ok()).collect())
            .unwrap_or_default()
    }
}

/// Unwrap the v2 JSON envelope `{"type":..,"key":".."}`.
/// Falls back to the raw string when it is not JSON (forward compatible).
pub fn unwrap_envelope(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(k) = v.get("key").and_then(|k| k.as_str()) {
            if !k.is_empty() {
                return k.to_string();
            }
        }
    }
    raw.to_string()
}

/// Fetch the enabled model catalog via the OpenCode CLI (service-auth aware).
pub fn fetch_catalog(opencode_bin: &str) -> Result<Vec<CatalogEntry>, String> {
    let out = Command::new(opencode_bin)
        .args(["api", "get", "/api/model"])
        .output()
        .map_err(|e| format!("failed to run `{opencode_bin} api get /api/model`: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "opencode api failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("invalid catalog JSON: {e}"))?;
    let data = v.get("data").cloned().unwrap_or(v);
    let entries: Vec<CatalogEntry> =
        serde_json::from_value(data).map_err(|e| format!("catalog shape: {e}"))?;
    Ok(entries.into_iter().filter(|e| e.enabled).collect())
}

/// Upstream auth for a catalog entry:
/// - provider `opencode` (free tier) uses the public key from settings.
/// - everything else uses the stored credential for that integration id.
pub fn upstream_bearer(entry: &CatalogEntry, store: &CredentialStore) -> Option<SecretString> {
    if entry.provider_id == "opencode" {
        if let Some(k) = entry.settings.api_key.clone() {
            if !k.is_empty() {
                return Some(SecretString::from(k));
            }
        }
        return Some(SecretString::from("public".to_string()));
    }
    // The integration id matches the provider id (e.g. opencode-go).
    // Fall back to the settings.provider hint when present.
    if let Some(s) = store.get(&entry.provider_id) {
        return Some(s);
    }
    if let Some(hint) = entry.settings.provider.as_deref() {
        if hint != entry.provider_id {
            return store.get(hint);
        }
    }
    None
}

#[allow(dead_code)]
pub fn debug_bearer_len(b: Option<&SecretString>) -> usize {
    b.map(|s| s.expose_secret().len()).unwrap_or(0)
}
