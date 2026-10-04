//! Gateway state: catalog + alias map, model resolution and boot retry.
//!
//! `AppState` is the single source of truth handlers share; `refresh()`
//! rebuilds it from the OpenCode catalog.

use crate::config::AppConfig;
use crate::domain::{
    auto_aliases_for, is_known_package, AliasEntry, AliasOptions, CatalogEntry, ModelRef,
};
use crate::infra::opencode::fetch_catalog;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

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

    /// Rebuild catalog + alias map. Manual aliases win; disabled are skipped.
    /// Console free-tier (`opencode/*`) models are skipped unless
    /// `include_free_tier` is set: they 403 outside OpenCode.
    /// Failures keep the previous catalog and are recorded for `/health`.
    pub async fn refresh(&self) -> Result<usize, String> {
        let bin = self.config.opencode_bin.clone();
        let fetch = tokio::task::spawn_blocking(move || fetch_catalog(&bin));
        let entries = match tokio::time::timeout(Duration::from_secs(20), fetch).await {
            Ok(Ok(Ok(entries))) => entries,
            Ok(Ok(Err(e))) => {
                let msg = format!("catalog fetch failed: {e}");
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
            Ok(Err(e)) => {
                let msg = format!("catalog task failed: {e}");
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
            Err(_) => {
                let msg = "catalog fetch timed out after 20s".to_string();
                *self.last_error.write().await = Some(msg.clone());
                return Err(msg);
            }
        };
        for e in &entries {
            tracing::debug!(
                provider_id = %e.provider_id,
                model_id = %e.model_id,
                package = %e.package,
                "catalog entry"
            );
            if !is_known_package(&e.package) {
                tracing::warn!(
                    provider_id = %e.provider_id,
                    package = %e.package,
                    "unknown provider package, using ChatCompletions fallback"
                );
            }
        }
        let usable: Vec<&CatalogEntry> = entries
            .iter()
            .filter(|e| self.config.include_free_tier || e.provider_id != "opencode")
            .collect();
        let mut aliases: Vec<AliasEntry> = vec![];
        let mut used_refs = std::collections::HashSet::new();

        // 1. Manual aliases first.
        let mut manual: Vec<AliasEntry> = self
            .config
            .aliases
            .iter()
            .map(|(gw, a)| {
                // Window lookup: manual `opencode` may be `qualified()`
                // (`provider/modelID`) or the preferred ref of an id-distinct
                // row (`provider/id`, e.g. `.../claude-opus-4.8-fast`) — match
                // both so fast flavors keep the catalog window (`[1m]`).
                let window = entries
                    .iter()
                    .find(|e| e.qualified() == a.opencode || e.preferred_ref() == a.opencode)
                    .and_then(|e| e.context_window());
                let mut alias = AliasEntry {
                    gateway_id: gw.clone(),
                    opencode_ref: a.opencode.clone(),
                    display_name: a.display_name.clone().unwrap_or_else(|| gw.clone()),
                    description: a
                        .description
                        .clone()
                        .unwrap_or_else(|| format!("via frank-opencode · {}", a.opencode)),
                    context_window: window,
                    family_tier: None,
                    family_default: false,
                };
                self.apply_tier(&mut alias);
                alias
            })
            .collect();
        manual.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
        for a in manual {
            if self.config.is_disabled(&a.opencode_ref, &a.gateway_id) {
                continue;
            }
            used_refs.insert(a.opencode_ref.clone());
            aliases.push(a);
        }
        // 2. Auto aliases for the rest (free-tier excluded unless opted in).
        // A manual alias may point at either `provider/model` or
        // `provider/id` (fast flavors), so exclude a catalog row when any of
        // its refs is taken.
        let remaining: Vec<CatalogEntry> = usable
            .iter()
            .filter(|e| {
                !used_refs.contains(&e.qualified())
                    && !used_refs.contains(&e.id_ref())
                    && !used_refs.contains(&e.preferred_ref())
            })
            .map(|e| (*e).clone())
            .collect();
        let mut auto: Vec<AliasEntry> = auto_aliases_for(
            &remaining,
            AliasOptions {
                evade: self.config.desktop_aliases,
                shield: self.config.cli_shield_aliases,
            },
        )
        .into_iter()
        .filter(|a| !self.config.is_disabled(&a.opencode_ref, &a.gateway_id))
        .collect();
        auto.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
        // Avoid gateway_id collisions with manual entries (and among autos:
        // `auto_aliases_for` already dedups, but manual ids win).
        let mut taken: std::collections::HashSet<String> =
            aliases.iter().map(|a| a.gateway_id.clone()).collect();
        for mut a in auto {
            self.apply_tier(&mut a);
            if taken.insert(a.gateway_id.clone()) {
                aliases.push(a);
            } else {
                tracing::warn!(
                    gateway_id = %a.gateway_id,
                    opencode_ref = %a.opencode_ref,
                    "duplicate gateway_id, skipping auto alias"
                );
            }
        }
        aliases.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));

        let n = usable.len();
        *self.catalog.write().await = entries;
        *self.aliases.write().await = aliases;
        *self.last_refresh.write().await = Some(SystemTime::now());
        *self.last_error.write().await = None;
        Ok(n)
    }

    /// Boot loader: retry `refresh()` until the catalog is non-empty or
    /// `attempts` are exhausted. `opencode api` can (re)start the OpenCode
    /// service, so the first fetch often returns an *empty* catalog (HTTP ok,
    /// `data: []`) while it warms up — without this the daemon would snapshot
    /// `models: 0` until a manual restart. Failures and empty loads keep the
    /// previous catalog and are recorded for `/health`; a non-empty load
    /// clears the error as usual.
    pub async fn refresh_with_retry(&self, attempts: usize, backoff: Duration) -> usize {
        let mut delay = backoff;
        // n<0 means no successful load; used only to disambiguate the final
        // message (failures vs. persistent empty).
        let mut n: i64 = -1;
        for attempt in 1..=attempts {
            match self.refresh().await {
                Ok(loaded) if loaded > 0 => return loaded,
                Ok(_) => {
                    n = 0;
                    let msg = format!(
                        "catalog loaded empty (attempt {attempt}/{attempts}); retrying in {}s",
                        delay.as_secs()
                    );
                    tracing::warn!(%msg);
                    *self.last_error.write().await = Some(msg);
                }
                Err(e) => {
                    tracing::warn!(err = %e, attempt, "catalog load attempt failed");
                }
            }
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, BOOT_CATALOG_MAX_BACKOFF);
        }
        // Always record the final state so /health never looks `ok` with an
        // empty catalog. `last_error` is Some -> status `degraded`.
        let total = (1..attempts)
            .fold(Duration::ZERO, |acc, i| {
                acc + std::cmp::min(backoff * 2u32.pow(i as u32), BOOT_CATALOG_MAX_BACKOFF)
            })
            .as_secs();
        let msg = if n < 0 {
            format!(
                "catalog fetch failed on all {attempts} attempts ({total}s of retries); \
                 check the OpenCode service (`opencode service status`) and restart the gateway"
            )
        } else {
            format!(
                "catalog still empty after {attempts} attempts ({total}s of retries); \
                 loaded{n} models — check the OpenCode service (`opencode service status`) and \
                 restart the gateway"
            )
        };
        *self.last_error.write().await = Some(msg.clone());
        tracing::warn!(%msg);
        n.max(0) as usize
    }

    /// Fill the Anthropic family tier on an alias from `[tiers]` config.
    /// Gateway id wins over the OpenCode ref; unmapped aliases keep no tier.
    fn apply_tier(&self, alias: &mut AliasEntry) {
        if let Some(t) = self.config.tier_for(&alias.opencode_ref, &alias.gateway_id) {
            alias.family_tier = Some(t.tier.as_str().to_string());
            alias.family_default = t.family_default;
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

    /// Resolve a requested model (gateway alias, `provider/model`, plain id,
    /// each optionally with a `#variant` suffix) to its catalog entry and the
    /// selected variant label. `Err` carries the 404 message.
    pub(crate) async fn resolve(
        &self,
        requested: &str,
    ) -> Result<(CatalogEntry, Option<String>), String> {
        let (base, variant) = match requested.split_once('#') {
            Some((b, v)) => (b.to_string(), Some(v.to_string())),
            None => (requested.to_string(), None),
        };
        let hit = self.lookup_entry(&base).await;
        self.finish_resolve(hit, &base, variant).await
    }

    /// Catalog lookup under the read guards (kept out of `resolve` so the
    /// variant checks in `finish_resolve` don't hold any lock).
    async fn lookup_entry(&self, base: &str) -> Option<CatalogEntry> {
        let catalog = self.catalog.read().await;
        let aliases = self.aliases.read().await;
        // Alias hit? Prefer the `id` match so disambiguated rows
        // (`provider/id`, e.g. `.../claude-opus-4.8-fast`) resolve to their
        // own headers/body instead of collapsing to the first row with the
        // same `modelID`.
        if let Some(a) = aliases.iter().find(|a| a.gateway_id == base) {
            if let Some(r) = ModelRef::parse(&a.opencode_ref) {
                if let Some(hit) = catalog
                    .iter()
                    .find(|e| e.provider_id == r.provider_id && e.id == r.model_id)
                    .cloned()
                {
                    return Some(hit);
                }
                if let Some(hit) = catalog
                    .iter()
                    .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                    .cloned()
                {
                    return Some(hit);
                }
            }
        }
        // Direct provider/model (or provider/id for fast flavors)?
        if let Some(r) = ModelRef::parse(base) {
            if let Some(hit) = catalog
                .iter()
                .find(|e| e.provider_id == r.provider_id && e.id == r.model_id)
                .cloned()
            {
                return Some(hit);
            }
            if let Some(hit) = catalog
                .iter()
                .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
                .cloned()
            {
                return Some(hit);
            }
        }
        // Plain model id (first enabled match)? Match `modelID` or `id`
        // (`claude-opus-4.8-fast` is an `id`, not a `modelID`). Sort for
        // determinism (ambiguity is order-dependent across providers) and warn.
        let mut hits: Vec<CatalogEntry> = catalog
            .iter()
            .filter(|e| e.model_id == base || e.id == base)
            .cloned()
            .collect();
        hits.sort_by_key(|e| e.qualified());
        if hits.len() > 1 {
            tracing::warn!(
                model = base,
                candidates = ?hits.iter().map(|e| e.qualified()).collect::<Vec<_>>(),
                "ambiguous model id, using first; prefer an alias or provider/model"
            );
        }
        hits.into_iter().next()
    }

    async fn finish_resolve(
        &self,
        entry: Option<CatalogEntry>,
        base: &str,
        variant: Option<String>,
    ) -> Result<(CatalogEntry, Option<String>), String> {
        let Some(entry) = entry else {
            return Err(format!("unknown model '{base}' (see GET /v1/models)"));
        };
        let v = match variant {
            None => None,
            Some(v) => {
                let known: Vec<&str> = entry.variants.iter().map(|x| x.id.as_str()).collect();
                if known.iter().any(|x| **x == v) {
                    Some(v)
                } else if known.is_empty() {
                    return Err(format!(
                        "model '{}' has no variants; requested '{}'",
                        entry.qualified(),
                        v
                    ));
                } else {
                    return Err(format!(
                        "model '{}' has no variant '{}' (available: {})",
                        entry.qualified(),
                        v,
                        known.join(", ")
                    ));
                }
            }
        };
        Ok((entry, v))
    }
}
