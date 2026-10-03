//! Model resolution: gateway alias / provider-model / plain id.

use crate::api::state::AppState;
use crate::domain::{CatalogEntry, GatewayError, ModelRef};

/// Find a catalog row by provider + model, preferring the `id` match so
/// disambiguated rows (`provider/id`, e.g. `.../claude-opus-4.8-fast`)
/// resolve to their own headers/body instead of collapsing to the first
/// row with the same `modelID`.
fn find_by_provider_model(catalog: &[CatalogEntry], r: &ModelRef) -> Option<CatalogEntry> {
    catalog
        .iter()
        .find(|e| e.provider_id == r.provider_id && e.id == r.model_id)
        .or_else(|| {
            catalog
                .iter()
                .find(|e| e.provider_id == r.provider_id && e.model_id == r.model_id)
        })
        .cloned()
}

impl AppState {
    /// Resolve a requested model (gateway alias, `provider/model`, plain id,
    /// each optionally with a `#variant` suffix) to its catalog entry and the
    /// selected variant label. `Err` carries the 404 details.
    async fn resolve(
        &self,
        requested: &str,
    ) -> Result<(CatalogEntry, Option<String>), GatewayError> {
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
                if let Some(hit) = find_by_provider_model(&catalog, &r) {
                    return Some(hit);
                }
            }
        }
        // Direct provider/model (or provider/id for fast flavors)?
        if let Some(r) = ModelRef::parse(base) {
            if let Some(hit) = find_by_provider_model(&catalog, &r) {
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
    ) -> Result<(CatalogEntry, Option<String>), GatewayError> {
        let Some(entry) = entry else {
            return Err(GatewayError::UnknownModel {
                name: base.to_string(),
            });
        };
        let v = match variant {
            None => None,
            Some(v) => {
                let known: Vec<&str> = entry.variants.iter().map(|x| x.id.as_str()).collect();
                if known.iter().any(|x| **x == v) {
                    Some(v)
                } else if known.is_empty() {
                    return Err(GatewayError::NoVariants {
                        model: entry.qualified(),
                        requested: v,
                    });
                } else {
                    return Err(GatewayError::UnknownVariant {
                        model: entry.qualified(),
                        requested: v,
                        available: known.iter().map(|s| s.to_string()).collect(),
                    });
                }
            }
        };
        Ok((entry, v))
    }
}
