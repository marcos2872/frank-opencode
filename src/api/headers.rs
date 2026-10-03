//! Outgoing upstream headers and catalog body defaults.

use crate::config::AppConfig;
use crate::daemon::write_private;
use crate::domain::CatalogEntry;
use axum::http::HeaderMap;
use serde_json::Value;
use std::sync::OnceLock;

/// Stable fallback session id (persisted) for clients that send no session
/// header (e.g. curl smoke tests). Go uses it for routing/prompt caching.
static FALLBACK_SESSION: OnceLock<String> = OnceLock::new();

fn fallback_session_id() -> String {
    FALLBACK_SESSION
        .get_or_init(|| {
            let path = AppConfig::data_dir().join("frank.session");
            if let Ok(s) = std::fs::read_to_string(&path) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            let id = format!("frank-{}", uuid::Uuid::new_v4());
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = write_private(path, &id);
            id
        })
        .clone()
}

/// Outgoing Go session headers derived from the incoming client headers.
/// Go recognizes Claude Code's native session header; always also send
/// `x-opencode-session` (required for routing).
pub(crate) fn session_headers(incoming: &HeaderMap) -> Vec<(String, String)> {
    let mut out = vec![];
    let claude = incoming
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let direct = incoming
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !claude.is_empty() {
        out.push(("x-claude-code-session-id".to_string(), claude.clone()));
    }
    let session = if !direct.is_empty() {
        direct
    } else if !claude.is_empty() {
        claude
    } else {
        fallback_session_id()
    };
    out.push(("x-opencode-session".to_string(), session));
    out
}

/// Catalog default headers for an entry, excluding auth (bearer is set from
/// the credential store). Keys are matched case-insensitively by callers.
pub(crate) fn entry_headers(entry: &CatalogEntry) -> Vec<(String, String)> {
    let Some(h) = entry.headers.as_ref() else {
        return vec![];
    };
    h.iter()
        .filter(|(k, _)| {
            let l = k.to_ascii_lowercase();
            l != "authorization" && l != "x-api-key"
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub(crate) fn entry_beta_header(entry: &CatalogEntry) -> Option<String> {
    entry.headers.as_ref().and_then(|h| {
        h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.clone())
    })
}

/// Merge catalog `body` defaults (e.g. `{"speed":"fast"}`) into the upstream
/// JSON. Catalog wins on key conflicts: these are model defaults the Go
/// backend would have applied.
pub(crate) fn apply_entry_body(target: &mut Value, entry: &CatalogEntry) {
    let (Some(t), Some(e)) = (
        target.as_object_mut(),
        entry.body.as_ref().and_then(|v| v.as_object()),
    ) else {
        return;
    };
    for (k, v) in e {
        t.insert(k.clone(), v.clone());
    }
}
