//! Catalog refresh: fetch, alias building and publication.

use crate::api::state::{AppState, BOOT_CATALOG_MAX_BACKOFF};
use crate::domain::{auto_aliases_for, is_known_package, AliasEntry, CatalogEntry, GatewayError};
use crate::infra::opencode::fetch_catalog;
use std::time::{Duration, SystemTime};

/// Fetch the enabled catalog with a 20s budget. Failures are recorded
/// for `/health` with the historical message texts; the previous catalog
/// is kept untouched by the caller.
async fn fetch_entries(state: &AppState) -> Result<Vec<CatalogEntry>, GatewayError> {
    let bin = state.config.opencode_bin.clone();
    let fetch = tokio::task::spawn_blocking(move || fetch_catalog(&bin));
    match tokio::time::timeout(Duration::from_secs(20), fetch).await {
        Ok(Ok(Ok(entries))) => Ok(entries),
        // `last_error` text is preserved verbatim (surfaced on `/health`):
        // only the `Err` payload changes from `String` to typed.
        Ok(Ok(Err(e))) => {
            *state.last_error.write().await = Some(format!("catalog fetch failed: {e}"));
            Err(e)
        }
        Ok(Err(e)) => {
            let err = GatewayError::CatalogTask {
                source: e.to_string(),
            };
            *state.last_error.write().await = Some(format!("catalog task failed: {e}"));
            Err(err)
        }
        Err(_) => {
            *state.last_error.write().await = Some(GatewayError::CatalogTimeout.to_string());
            Err(GatewayError::CatalogTimeout)
        }
    }
}

/// Debug-log every catalog row; warn on unknown packages (they still
/// route via the ChatCompletions fallback).
fn log_catalog_entries(entries: &[CatalogEntry]) {
    for e in entries {
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
}

/// Manual aliases from config win; disabled ones are skipped.
/// Returns the alias list plus the set of taken `opencode_ref`s.
fn build_manual_aliases(
    state: &AppState,
    entries: &[CatalogEntry],
) -> (Vec<AliasEntry>, std::collections::HashSet<String>) {
    let mut manual: Vec<AliasEntry> = state
        .config
        .aliases
        .iter()
        .map(|(gw, a)| {
            let window = entries
                .iter()
                .find(|e| e.qualified() == a.opencode)
                .and_then(|e| e.context_window());
            AliasEntry {
                gateway_id: gw.clone(),
                opencode_ref: a.opencode.clone(),
                display_name: a.display_name.clone().unwrap_or_else(|| gw.clone()),
                description: a
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("via frank-opencode · {}", a.opencode)),
                context_window: window,
            }
        })
        .collect();
    manual.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    let mut aliases: Vec<AliasEntry> = vec![];
    let mut used_refs = std::collections::HashSet::new();
    for a in manual {
        if state.config.is_disabled(&a.opencode_ref, &a.gateway_id) {
            continue;
        }
        used_refs.insert(a.opencode_ref.clone());
        aliases.push(a);
    }
    (aliases, used_refs)
}

/// Auto aliases for every usable row not covered by a manual alias.
/// A manual alias may point at either `provider/model` or `provider/id`
/// (fast flavors), so a row is skipped when any of its refs is taken.
/// Free-tier (`opencode/*`) rows are excluded unless opted in.
fn build_auto_aliases(
    state: &AppState,
    entries: &[CatalogEntry],
    used_refs: &std::collections::HashSet<String>,
    taken: &mut std::collections::HashSet<String>,
) -> Vec<AliasEntry> {
    let usable: Vec<&CatalogEntry> = entries
        .iter()
        .filter(|e| state.config.include_free_tier || e.provider_id != "opencode")
        .collect();
    let remaining: Vec<CatalogEntry> = usable
        .iter()
        .filter(|e| {
            !used_refs.contains(&e.qualified())
                && !used_refs.contains(&e.id_ref())
                && !used_refs.contains(&e.preferred_ref())
        })
        .map(|e| (*e).clone())
        .collect();
    let mut auto: Vec<AliasEntry> = auto_aliases_for(&remaining, state.config.desktop_aliases)
        .into_iter()
        .filter(|a| !state.config.is_disabled(&a.opencode_ref, &a.gateway_id))
        .collect();
    auto.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));
    // Avoid gateway_id collisions with manual entries (and among autos:
    // `auto_aliases_for` already dedups, but manual ids win).
    let mut fresh: Vec<AliasEntry> = vec![];
    for a in auto {
        if taken.insert(a.gateway_id.clone()) {
            fresh.push(a);
        } else {
            tracing::warn!(
                gateway_id = %a.gateway_id,
                opencode_ref = %a.opencode_ref,
                "duplicate gateway_id, skipping auto alias"
            );
        }
    }
    fresh
}

/// Rebuild catalog + alias map. Manual aliases win; disabled are skipped.
/// Console free-tier (`opencode/*`) models are skipped unless
/// `include_free_tier` is set: they 403 outside OpenCode.
/// Failures keep the previous catalog and are recorded for `/health`.
pub async fn refresh_catalog(state: &AppState) -> Result<usize, GatewayError> {
    let entries = fetch_entries(state).await?;
    log_catalog_entries(&entries);
    let (mut aliases, used_refs) = build_manual_aliases(state, &entries);
    let mut taken: std::collections::HashSet<String> =
        aliases.iter().map(|a| a.gateway_id.clone()).collect();
    aliases.extend(build_auto_aliases(state, &entries, &used_refs, &mut taken));
    aliases.sort_by(|a, b| a.gateway_id.cmp(&b.gateway_id));

    let n = entries
        .iter()
        .filter(|e| state.config.include_free_tier || e.provider_id != "opencode")
        .count();
    *state.catalog.write().await = entries;
    *state.aliases.write().await = aliases;
    *state.last_refresh.write().await = Some(SystemTime::now());
    *state.last_error.write().await = None;
    Ok(n)
}

impl AppState {
    pub async fn refresh(&self) -> Result<usize, GatewayError> {
        refresh_catalog(self).await
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
}
