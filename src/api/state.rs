//! Gateway state: shared catalog, aliases and HTTP client.

use crate::config::AppConfig;
use crate::domain::{AliasEntry, CatalogEntry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// Idle gap after which translated/passthrough SSE streams emit `ping`
/// (keeps Claude Code's stream watchdog fed during long reasoning pauses).
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(20);

/// Boot catalog loader: number of `refresh()` calls before giving up.
/// `opencode api` can (re)start the OpenCode service and the first fetch often
/// returns an *empty* catalog (HTTP ok, `data: []`) while it warms up, so the
/// daemon retries instead of snapshotting `models: 0` forever.
pub const BOOT_CATALOG_ATTEMPTS: usize = 6;
/// Initial delay between boot catalog attempts (doubles up to
/// `BOOT_CATALOG_MAX_BACKOFF`). Tolerates a service still warming up while
/// keeping total boot delay bounded.
pub const BOOT_CATALOG_BACKOFF: Duration = Duration::from_secs(2);
pub const BOOT_CATALOG_MAX_BACKOFF: Duration = Duration::from_secs(16);

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub catalog: Arc<RwLock<Vec<CatalogEntry>>>,
    pub aliases: Arc<RwLock<Vec<AliasEntry>>>,
    pub last_refresh: Arc<RwLock<Option<SystemTime>>>,
    pub last_error: Arc<RwLock<Option<String>>>,
    pub http: reqwest::Client,
    pub db_path: PathBuf,
}

impl AppState {
    pub fn new(config: AppConfig, db_path: PathBuf) -> Self {
        let http = reqwest::Client::builder()
            // Connect/headers budget is short (fail fast on an unreachable
            // provider); the total timeout is generous because it also bounds
            // streaming responses, and a long reasoning turn can legitimately
            // stay open for many minutes.
            .connect_timeout(std::time::Duration::from_secs(config.connect_timeout_secs))
            .timeout(std::time::Duration::from_secs(config.request_timeout_secs))
            .user_agent(format!("frank-opencode/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Self {
            config,
            catalog: Arc::new(RwLock::new(vec![])),
            aliases: Arc::new(RwLock::new(vec![])),
            last_refresh: Arc::new(RwLock::new(None)),
            last_error: Arc::new(RwLock::new(None)),
            http,
            db_path,
        }
    }

    /// Single source of truth for the default model (config or first alias).
    pub async fn effective_default(&self) -> String {
        if !self.config.default_model.is_empty() {
            return self.config.default_model.clone();
        }
        self.aliases
            .read()
            .await
            .first()
            .map(|a| a.gateway_id.clone())
            .unwrap_or_default()
    }

}
