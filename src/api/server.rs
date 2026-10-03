//! HTTP layer: Axum handlers. Business decisions live in domain/infra.

use crate::config::AppConfig;
use crate::domain::{
    auto_aliases_for, is_known_package, protocol_for_entry, strip_window_suffix, window_suffix,
    AliasEntry, AliasOptions, CatalogEntry, ModelRef, Protocol,
};
use crate::infra::opencode::{fetch_catalog, upstream_bearer, CredentialStore};
use crate::infra::upstream::{
    anthropic_to_openai, anthropic_to_responses, apply_variant, apply_variant_checked,
    estimate_tokens, join_url, openai_to_anthropic, responses_to_anthropic, sse, sse_error,
    with_heartbeat, ResponsesTranslator, StreamTranslator,
};
use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use secrecy::ExposeSecret;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
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
    async fn resolve(&self, requested: &str) -> Result<(CatalogEntry, Option<String>), String> {
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

const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;

fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": err_type, "message": msg}});
    (status, Json(body)).into_response()
}

fn upstream_error_response(status: StatusCode, text: &str) -> Response {
    let value = serde_json::from_str::<Value>(text).ok();
    let message = value
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| body_preview(text));
    let error_type = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "authentication_error",
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => "invalid_request_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        _ => "api_error",
    };
    anthropic_error(status, error_type, &message)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", post(messages))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Gateway credential check. `/health` stays open (daemon probes + `enable`
/// waiter are unauthenticated); everything else requires the configured
/// `auth_token` in either `x-api-key` or `Authorization: Bearer` when set.
async fn require_token(
    State(s): State<AppState>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if req.uri().path() == "/health" || s.config.auth_token.is_empty() {
        return next.run(req).await;
    }
    let headers = req.headers();
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if bearer == s.config.auth_token || key == s.config.auth_token {
        next.run(req).await
    } else {
        anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid gateway credential (check ANTHROPIC_AUTH_TOKEN)",
        )
    }
}

async fn health(State(s): State<AppState>) -> impl IntoResponse {
    // Short-lived locks only: `effective_default()` takes `aliases` itself,
    // so don't hold that guard across the call.
    let model_count = s.aliases.read().await.len();
    let last_error = s.last_error.read().await.clone();
    let refreshed = s.last_refresh.read().await.is_some();
    let default_model = s.effective_default().await;
    let status = if last_error.is_some() {
        "degraded"
    } else if refreshed {
        "ok"
    } else {
        "starting"
    };
    Json(serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "default_model": default_model,
        "models": model_count,
        "last_error": last_error,
    }))
}

async fn list_models(State(s): State<AppState>) -> impl IntoResponse {
    let aliases = s.aliases.read().await;
    let data: Vec<Value> = aliases
        .iter()
        .map(|a| {
            let mut item = serde_json::json!({
                "id": a.gateway_id,
                "display_name": a.display_name,
                "description": a.description,
                "owned_by": "frank-opencode",
            });
            // Context window from the OpenCode catalog (`limit.context`).
            // Omitted when unknown so clients fall back to their default.
            if let Some(w) = a.context_window {
                item["context_window"] = Value::from(w);
                // Mainline Claude Code reads a window only from the literal
                // `[1m]` suffix on the id (never arbitrary `[<n>k]`), so only
                // windows >= 1M get a suffix. `resolve()` strips it before
                // matching, so the internal gateway_id is untouched.
                if let Some(sfx) = window_suffix(w) {
                    item["id"] = Value::String(format!("{a}{sfx}", a = a.gateway_id));
                }
            }
            // Anthropic family tier from `[tiers]` config. Claude Desktop's
            // `small_fast` background class (session titles) picks the first
            // `haiku` model; without a tier it falls back to id substring
            // matching and lands on the first `*sonnet*` row. `is_family_default`
            // is only honored by the Desktop together with a tier, so both
            // are emitted (or neither) for the flagged alias.
            if let Some(tier) = &a.family_tier {
                item["anthropic_family_tier"] = Value::String(tier.clone());
                if a.family_default {
                    item["is_family_default"] = Value::Bool(true);
                }
            }
            item
        })
        .collect();
    Json(serde_json::json!({"object": "list", "data": data}))
}

use serde_json::Value;

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
            let _ = crate::daemon::write_private(path, &id);
            id
        })
        .clone()
}

/// Outgoing Go session headers derived from the incoming client headers.
/// Go recognizes Claude Code's native session header; always also send
/// `x-opencode-session` (required for routing).
fn session_headers(incoming: &HeaderMap) -> Vec<(String, String)> {
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

/// `POST /v1/messages/count_tokens`: validate like the Messages endpoint,
/// then return the Anthropic count when the upstream package is Anthropic
/// (proxy), else the local per-part estimate. The upstream count_tokens
/// endpoint exists only in the Anthropic wire protocol.
async fn count_tokens(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    // Model resolution first: same 404/400 semantics as /v1/messages.
    let raw = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let requested = if raw.is_empty() {
        s.effective_default().await
    } else {
        strip_window_suffix(&raw).to_string()
    };
    if requested.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let (entry, variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(msg) => return anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg),
    };
    // A count without messages is meaningless.
    if body.get("messages").and_then(|m| m.as_array()).is_none() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing messages",
        );
    }

    if let Some(base) = entry.base_url() {
        if protocol_for_entry(&entry) == Protocol::Anthropic {
            if let Some(n) =
                proxy_count_tokens(&s, &headers, base, &entry, &body, variant.as_deref()).await
            {
                return Json(serde_json::json!({"input_tokens": n})).into_response();
            }
            // Proxy failed: fall through to the estimate.
        }
    }
    let n = estimate_tokens(&body);
    Json(serde_json::json!({"input_tokens": n})).into_response()
}

/// Ask `{baseURL}/messages/count_tokens` and return the token count.
/// Fallback to `None` on any transport/parse error so callers degrade to the
/// local estimate (the endpoint is optional in this gateway's contract).
async fn proxy_count_tokens(
    s: &AppState,
    headers: &HeaderMap,
    base: &str,
    entry: &CatalogEntry,
    body: &Value,
    variant: Option<&str>,
) -> Option<u64> {
    let store = CredentialStore::new(s.db_path.clone());
    let bearer = upstream_bearer(entry, &store)?;
    let mut req_body = body.clone();
    // The upstream counts under its own model id, not the gateway alias.
    req_body["model"] = Value::String(entry.model_id.clone());
    let url = join_url(base, "messages/count_tokens");
    // Client-only `anthropic-beta` here (no catalog merge: unlike the
    // Messages forward, the count proxy never merges fast-mode betas).
    let mut req = upstream_post(s, &url, &bearer).header(
        "anthropic-version",
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("2023-06-01"),
    );
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        req = req.header("anthropic-beta", beta);
    }
    let req = with_session_headers(
        with_entry_headers_except(req, entry, &["anthropic-beta"]),
        headers,
    );
    req_body.as_object_mut().map(|o| o.remove("stream"));
    // Apply the selected variant and the catalog body defaults, so the count
    // matches what the real forward would send (thinking changes the input
    // token count on the Messages API).
    if let Some(v) = variant {
        match apply_variant_checked(std::mem::take(&mut req_body), entry, v) {
            Ok(applied) => req_body = applied,
            Err(_) => return None,
        }
    }
    apply_entry_body(&mut req_body, entry);
    let resp = req.json(&req_body).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("input_tokens").and_then(|n| n.as_u64())
}

async fn messages(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let raw = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    // Accept Claude Code's `[1m]`/`[200k]` window hints on unknown gateway ids.
    let requested = if raw.is_empty() {
        s.effective_default().await
    } else {
        strip_window_suffix(&raw).to_string()
    };
    if requested.is_empty() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "missing model (and no default_model configured)",
        );
    }
    let stream = body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Local mock for Claude Code's auto-mode safety classifier (and its
    // liveness probes): answer here so the check never reaches an upstream
    // that may be out of quota. Intercepts before resolve, so it works for
    // any id; real conversations carry tools and are not matched.
    if s.config.mock_classifier && !stream {
        if let Some(mocked) = mock_check_response(&body, &requested) {
            tracing::info!(model = %requested, "mocked classifier/probe request");
            return Json(mocked).into_response();
        }
    }
    let (entry, variant) = match s.resolve(&requested).await {
        Ok(ok) => ok,
        Err(msg) => {
            return anthropic_error(StatusCode::NOT_FOUND, "not_found_error", &msg);
        }
    };

    let store = CredentialStore::new(s.db_path.clone());
    let Some(bearer) = upstream_bearer(&entry, &store) else {
        return anthropic_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            &format!(
                "no stored credential for '{}' (run `opencode auth login`)",
                entry.provider_id
            ),
        );
    };

    let Some(base) = entry.base_url().map(|x| x.to_string()) else {
        return anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("provider '{}' has no baseURL", entry.provider_id),
        );
    };

    match protocol_for_entry(&entry) {
        Protocol::Anthropic => {
            forward_anthropic(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
        Protocol::Responses => {
            forward_responses(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
        Protocol::ChatCompletions => {
            forward_openai(ForwardCtx {
                s: &s,
                headers: &headers,
                body: &body,
                entry: &entry,
                base: &base,
                bearer: &bearer,
                gateway_model: &requested,
                stream,
                variant: variant.as_deref(),
            })
            .await
        }
    }
}

/// Truncate an upstream error body for logs (never log credentials here;
///
/// callers only pass status + body, never the bearer).
/// Message from a Responses `response.failed` event: the upstream reports the
/// failure under `response.error` (falling back to a few legacy shapes).
fn response_failure_message(v: &Value) -> String {
    let r = v.get("response").unwrap_or(v);
    let picked = r
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| r.get("error").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| {
            r.get("incomplete_details")
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match picked {
        Some(s) if !s.is_empty() => format!("upstream response failed: {s}"),
        _ => "upstream response failed".to_string(),
    }
}

fn body_preview(s: &str) -> String {
    const MAX: usize = 500;
    let t = s.trim();
    if t.len() <= MAX {
        return t.to_string();
    }
    let end = t
        .char_indices()
        .take_while(|(index, _)| *index < MAX)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0);
    let mut out = t[..end].to_string();
    out.push('…');
    out
}

fn log_upstream_error(
    entry: &CatalogEntry,
    gateway_model: &str,
    base: &str,
    status: StatusCode,
    text: &str,
    req_summary: &serde_json::Value,
    headers: &HeaderMap,
) {
    // Client identification (no prompt content): which app/CLI sent the
    // request and from which session — enough to attribute background flows
    // (title-gen, classifier, probes) that pick a model on their own.
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    tracing::warn!(
        gateway_model = %gateway_model,
        opencode_ref = %entry.qualified(),
        entry_id = %entry.id,
        provider = %entry.provider_id,
        base_url = %base,
        status = status.as_u16(),
        body = %body_preview(text),
        user_agent = %header("user-agent"),
        session = %header("x-claude-code-session-id"),
        req = %req_summary,
        "upstream rejected request"
    );
}

/// Privacy-safe shape of the Anthropic request that failed: counts, block
/// kinds and sizes only, never prompt/tool content. Lets us tell "poisoned
/// history in this session" (e.g. a tool_result shape the translator or the
/// upstream rejects) apart from "model down" without logging user data.
fn request_summary(body: &Value) -> Value {
    let mut msgs = vec![];
    if let Some(arr) = body.get("messages").and_then(|m| m.as_array()) {
        for m in arr.iter().take(50) {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            match m.get("content") {
                Some(Value::String(s)) => msgs.push(serde_json::json!({
                    "role": role, "kind": "text", "chars": s.chars().count()
                })),
                Some(Value::Array(blocks)) => {
                    let kinds: Vec<String> = blocks
                        .iter()
                        .map(|b| {
                            let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                            // Text length without content: tool I/O still needs a size hint.
                            let chars = b
                                .get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.chars().count())
                                .unwrap_or(0);
                            if chars > 0 {
                                format!("{t}:{chars}ch")
                            } else {
                                t.to_string()
                            }
                        })
                        .collect();
                    msgs.push(serde_json::json!({"role": role, "kind": kinds}));
                }
                _ => msgs.push(serde_json::json!({"role": role, "kind": "other"})),
            }
        }
    }
    let tools: Vec<String> = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .take(30)
                .map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "messages": msgs.len(),
        "detail": msgs,
        "tools": tools,
        "tool_choice": body.get("tool_choice").and_then(|t| t.get("type").and_then(|x| x.as_str())),
        "max_tokens": body.get("max_tokens"),
        "stream": body.get("stream"),
        "system": body.get("system").map(|s| if s.is_string() { "text" } else { "blocks" }),
    })
}

/// Flatten the request `system` (string or content blocks) into plain text.
fn system_text(body: &Value) -> Option<String> {
    match body.get("system")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Anthropic-shaped message for a locally answered check, mirroring the
/// non-streaming response shape the translators build.
fn mock_message(text: &str, gateway_model: &str) -> Value {
    serde_json::json!({
        "id": format!("msg_{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..24]),
        "type": "message",
        "role": "assistant",
        "model": gateway_model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 0, "output_tokens": 1}
    })
}

/// Detect a Claude Code auto-mode safety-classifier call (or its tiny
/// liveness probe) and build the canned local reply.
///
/// Detection reads the `system` prompt only — never user messages — so a
/// conversation that merely quotes the classifier tags is not swallowed;
/// an empty/absent `tools` array is a second gate (real agent turns always
/// carry tools). Stage 1 wants `<severity>` alone, stage 2 wants the reply
/// to *begin* with `<block>`, so the ambiguous both-tags reply is
/// block-first. Tradeoff: with `mock_classifier` on, auto mode's LLM
/// safety review always comes back "allow".
fn mock_check_response(body: &Value, gateway_model: &str) -> Option<Value> {
    let tools_empty = match body.get("tools") {
        None => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    };
    if !tools_empty {
        return None;
    }
    // Liveness probe: no system, a single tiny message, max_tokens ≤ 4.
    if body.get("system").is_none()
        && body
            .get("max_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
            <= 4
    {
        if let Some(msgs) = body.get("messages").and_then(Value::as_array) {
            let tiny = msgs.len() == 1
                && msgs[0]
                    .get("content")
                    .and_then(Value::as_str)
                    .map(|s| s.chars().count() <= 4)
                    .unwrap_or(false);
            if tiny {
                return Some(mock_message("ok", gateway_model));
            }
        }
    }
    let sys = system_text(body)?;
    let has_block = sys.contains("<block>");
    let has_severity = sys.contains("<severity>");
    let text = match (has_block, has_severity) {
        (true, true) => "<block>no</block><severity>0</severity>",
        (true, false) => "<block>no</block>",
        (false, true) => "<severity>0</severity>",
        (false, false) => return None,
    };
    Some(mock_message(text, gateway_model))
}

/// Catalog default headers for an entry, excluding auth (bearer is set from
/// the credential store). Keys are matched case-insensitively by callers.
fn entry_headers(entry: &CatalogEntry) -> Vec<(String, String)> {
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

fn entry_beta_header(entry: &CatalogEntry) -> Option<String> {
    entry.headers.as_ref().and_then(|h| {
        h.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, v)| v.clone())
    })
}

/// Merge catalog `body` defaults (e.g. `{"speed":"fast"}`) into the upstream
/// JSON. Catalog wins on key conflicts: these are model defaults the Go
/// backend would have applied.
fn apply_entry_body(target: &mut Value, entry: &CatalogEntry) {
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

/// Shared upstream plumbing for the `forward_*` functions and
/// `proxy_count_tokens`, so a change to auth headers, error mapping or SSE
/// framing is made once instead of N times. Protocol-specific headers
/// (Anthropic version/beta) stay in their forward; everything identical across
/// paths lives here.
///
/// Base POST every upstream forward starts from: content-type plus the
/// bearer credential (both header shapes the backends accept).
fn upstream_post(
    s: &AppState,
    url: &str,
    bearer: &secrecy::SecretString,
) -> reqwest::RequestBuilder {
    s.http
        .post(url)
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {}", bearer.expose_secret()),
        )
        .header("x-api-key", bearer.expose_secret().to_string())
}

/// Catalog default headers for this entry (auth excluded: the bearer above
/// is the credential), skipping any header in `skip` (case-insensitive).
/// The Anthropic paths handle `anthropic-beta` themselves (client/catalog
/// merge in the forward, client-only in the count_tokens proxy) and skip it
/// here; the translated forwards pass no skips.
fn with_entry_headers_except(
    req: reqwest::RequestBuilder,
    entry: &CatalogEntry,
    skip: &[&str],
) -> reqwest::RequestBuilder {
    let mut req = req;
    for (k, v) in entry_headers(entry) {
        if skip.iter().any(|s| k.eq_ignore_ascii_case(s)) {
            continue;
        }
        req = req.header(k, v);
    }
    req
}

/// Catalog default headers for this entry (auth excluded: the bearer above
/// is the credential).
fn with_entry_headers(
    req: reqwest::RequestBuilder,
    entry: &CatalogEntry,
) -> reqwest::RequestBuilder {
    with_entry_headers_except(req, entry, &[])
}

/// Session routing headers for this client (always sent).
fn with_session_headers(
    req: reqwest::RequestBuilder,
    headers: &HeaderMap,
) -> reqwest::RequestBuilder {
    let mut req = req;
    for (k, v) in session_headers(headers) {
        req = req.header(k, v);
    }
    req
}

/// POST a JSON body; a transport failure is already the gateway error
/// response (`upstream unreachable`), so callers just early-return it.
/// The error is boxed: an inline `Response` would trip
/// `clippy::result_large_err` (a `Response<Body>` is a large variant).
async fn send_json(
    req: reqwest::RequestBuilder,
    body: &Value,
) -> Result<reqwest::Response, Box<Response>> {
    match req.json(body).send().await {
        Ok(r) => Ok(r),
        Err(e) => Err(Box::new(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("upstream unreachable: {e}"),
        ))),
    }
}

/// A non-2xx upstream status: log the privacy-safe summary and return the
/// normalized Anthropic error shape. `summary_body` is whatever the caller
/// logged before (the Anthropic forward logs its mutated body, the translated
/// forwards log the original client body).
fn log_and_map_upstream_error(
    entry: &CatalogEntry,
    gateway_model: &str,
    base: &str,
    status: StatusCode,
    text: String,
    summary_body: &Value,
    headers: &HeaderMap,
) -> Response {
    let summary = request_summary(summary_body);
    log_upstream_error(entry, gateway_model, base, status, &text, &summary, headers);
    upstream_error_response(status, &text)
}

/// Read an upstream JSON body; invalid JSON is already the gateway error.
/// The error is boxed, same as `send_json` above.
async fn read_upstream_json(resp: reqwest::Response) -> Result<Value, Box<Response>> {
    match resp.json().await {
        Ok(v) => Ok(v),
        Err(e) => Err(Box::new(anthropic_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("invalid upstream JSON: {e}"),
        ))),
    }
}

/// Drain complete lines from the SSE byte buffer, returning the `data:`
/// payloads. Empty lines, non-`data:` lines and `[DONE]` are skipped, exactly
/// as each forward did inline before.
fn drain_sse_payloads(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = vec![];
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line);
        let t = text.trim();
        let payload = t.strip_prefix("data:").map(|x| x.trim()).unwrap_or("");
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        out.push(payload.to_string());
    }
    out
}

/// The empty text-block start both translated streams emit when the upstream
/// never opened any content, so the stream stays well-formed.
fn empty_text_block_event() -> String {
    sse(
        &serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    )
}

/// Terminal error event both translated streams emit when the upstream fails
/// after the stream opened, instead of ending 200 with no explanation.
fn terminal_error_event(msg: &str) -> String {
    sse_error("api_error", msg)
}

/// Final SSE response every streaming forward returns: 200 with heartbeat
/// pings feeding the client's stream watchdog.
fn sse_stream_response<S>(out: S) -> Response
where
    S: futures::Stream<Item = Result<Vec<u8>, std::io::Error>> + Send + 'static,
{
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::from_stream(with_heartbeat(out, HEARTBEAT_IDLE)))
        .unwrap()
}

/// Shared context for the three protocol `forward_*` functions: everything
/// `messages()` resolved that a forward needs. A struct (not 9 positional
/// args) so a new field doesn't become a tenth parameter.
struct ForwardCtx<'a> {
    s: &'a AppState,
    headers: &'a HeaderMap,
    body: &'a Value,
    entry: &'a CatalogEntry,
    base: &'a str,
    bearer: &'a secrecy::SecretString,
    gateway_model: &'a str,
    stream: bool,
    variant: Option<&'a str>,
}

async fn forward_anthropic(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // The Messages API forwards client fields verbatim, so work on a copy:
    // nothing after the forward reads the caller's body back.
    let mut body = body.clone();
    let body = &mut body;
    body["model"] = Value::String(entry.model_id.clone());
    // Apply the selected variant to the raw body: the Messages API forwards
    // client fields verbatim, so `thinking`/`include` land unchanged. A
    // variant the Messages API cannot represent is a 400, not a silent no-op.
    if let Some(v) = variant {
        match apply_variant_checked(std::mem::take(body), entry, v) {
            Ok(applied) => *body = applied,
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &e.to_string(),
                )
            }
        }
    }
    apply_entry_body(body, entry);
    // `stream` passthrough stays as the client sent it.
    let url = join_url(base, "messages");
    // Only the Messages API merges catalog betas into the client's
    // `anthropic-beta` (fast-mode): both are comma-separated lists.
    let mut req = upstream_post(s, &url, bearer).header(
        "anthropic-version",
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("2023-06-01"),
    );
    if let Some(beta) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        let merged = match entry_beta_header(entry) {
            Some(catalog) if !catalog.is_empty() && !beta.contains(&catalog) => {
                format!("{beta}, {catalog}")
            }
            _ => beta.to_string(),
        };
        req = req.header("anthropic-beta", merged);
    } else if let Some(catalog) = entry_beta_header(entry) {
        req = req.header("anthropic-beta", catalog);
    }
    let req = with_session_headers(
        with_entry_headers_except(req, entry, &["anthropic-beta"]),
        headers,
    );
    let resp = match send_json(req, body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if stream {
        // Byte passthrough, but inject `ping` during upstream silence so the
        // client's stream watchdog doesn't abort long thinking pauses.
        let stream = with_heartbeat(
            resp.bytes_stream()
                .map(|c| c.map(|b| b.to_vec()).map_err(std::io::Error::other)),
            HEARTBEAT_IDLE,
        );
        sse_stream_response(stream)
    } else {
        // Rewrite `model` to the gateway id for a consistent client view.
        let mut v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
        };
        if v.get("model").is_some() {
            v["model"] = Value::String(gateway_model.to_string());
        }
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
        }
        Json(v).into_response()
    }
}

async fn forward_responses(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut resp_body = if let Some(v) = variant {
        apply_variant(anthropic_to_responses(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_responses(body, &entry.model_id)
    };
    apply_entry_body(&mut resp_body, entry);
    let url = join_url(base, "responses");
    let req = with_session_headers(
        with_entry_headers(upstream_post(s, &url, bearer), entry),
        headers,
    );
    let resp = match send_json(req, &resp_body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if !stream {
        let v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
        };
        if let Some(uid) = v.get("id").and_then(|x| x.as_str()) {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
        }
        return Json(responses_to_anthropic(&v, gateway_model)).into_response();
    }
    // Translated streaming: Responses SSE -> Anthropic SSE.
    let gw = gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = ResponsesTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        let mut pinned = Box::pin(byte_stream);
        let mut output_tokens: u64 = 0;
        let mut input_tokens: u64 = 0;
        let mut incomplete = false;
        let mut upstream_error: Option<String> = None;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    upstream_error = Some(format!("upstream stream read failed: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            for payload in drain_sse_payloads(&mut buf) {
                let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue; };
                // Usage + status arrive on response.completed.
                if let Some(r) = v.get("response") {
                    if let Some(u) = r.get("usage") {
                        if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                            input_tokens = n;
                            tr.input_tokens = n;
                        }
                        if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
                            output_tokens = n;
                        }
                    }
                }
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("response.incomplete") => incomplete = true,
                    // The upstream failed after the stream opened: surface it
                    // rather than ending 200 with no explanation.
                    Some("response.failed") => {
                        upstream_error = Some(response_failure_message(&v));
                    }
                    _ => {}
                }
                for ev in tr.feed(&v) {
                    yield Ok(ev.into_bytes());
                }
            }
        }
        if !tr.text_open && !tr.has_tools() {
            yield Ok(empty_text_block_event().into_bytes());
            tr.text_open = true;
        }
        let reason = if tr.has_tools() {
            "tool_use"
        } else if incomplete {
            "max_tokens"
        } else {
            "end_turn"
        };
        for line in tr.finish(reason, input_tokens, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = upstream_error {
            yield Ok::<_, std::io::Error>(terminal_error_event(&msg).into_bytes());
        }
    };
    sse_stream_response(out)
}

async fn forward_openai(ctx: ForwardCtx<'_>) -> Response {
    let ForwardCtx {
        s,
        headers,
        body,
        entry,
        base,
        bearer,
        gateway_model,
        stream,
        variant,
    } = ctx;
    // Apply the selected variant to the translated body, not the raw
    // client body: the translators drop unknown fields.
    let mut oai_body = if let Some(v) = variant {
        apply_variant(anthropic_to_openai(body, &entry.model_id), entry, v)
    } else {
        anthropic_to_openai(body, &entry.model_id)
    };
    apply_entry_body(&mut oai_body, entry);
    let url = join_url(base, "chat/completions");
    // Some OpenAI-compatible gateways also accept api-key header; harmless to send.
    let req = with_session_headers(
        with_entry_headers(upstream_post(s, &url, bearer), entry),
        headers,
    );
    let resp = match send_json(req, &oai_body).await {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return log_and_map_upstream_error(entry, gateway_model, base, status, text, body, headers);
    }
    if !stream {
        let v: Value = match read_upstream_json(resp).await {
            Ok(v) => v,
            Err(e) => return *e,
        };
        if let Some(uid) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_str())
        {
            tracing::debug!(upstream_id = uid, model = gateway_model, "upstream ok");
        }
        return Json(openai_to_anthropic(&v, gateway_model)).into_response();
    }
    // Translated streaming: OpenAI SSE -> Anthropic SSE.
    let gw = gateway_model.to_string();
    let byte_stream = resp.bytes_stream();
    let out = async_stream::stream! {
        let mut tr = StreamTranslator::new(&gw);
        for line in tr.prefix() { yield Ok::<_, std::io::Error>(line.into_bytes()); }
        let mut buf: Vec<u8> = vec![];
        use futures::StreamExt;
        let mut pinned = Box::pin(byte_stream);
        let mut output_tokens: u64 = 0;
        let mut input_tokens: u64 = 0;
        let mut stop_reason = "end_turn".to_string();
        let mut upstream_error: Option<String> = None;
        while let Some(chunk) = pinned.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    upstream_error = Some(format!("upstream stream read failed: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&bytes);
            for payload in drain_sse_payloads(&mut buf) {
                let Ok(v) = serde_json::from_str::<Value>(&payload) else { continue; };
                if let Some(u) = v.get("usage") {
                    if let Some(n) = u.get("prompt_tokens").and_then(Value::as_u64) {
                        input_tokens = n;
                        tr.input_tokens = n;
                    }
                    if let Some(n) = u.get("completion_tokens").and_then(Value::as_u64) {
                        output_tokens = n;
                    }
                }
                if let Some(fr) = v.get("choices").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|c| c.get("finish_reason")).and_then(|f| f.as_str()) {
                    stop_reason = match fr {
                        "tool_calls" => "tool_use".to_string(),
                        "length" => "max_tokens".to_string(),
                        _ => "end_turn".to_string(),
                    };
                }
                // tool_calls finish without content still needs block emission (handled in feed).
                for ev in tr.feed(&v) {
                    yield Ok(ev.into_bytes());
                }
            }
        }
        // If the upstream never opened a text block but we have no content,
        // open/close an empty one so the stream is well-formed.
        if !tr.text_open && tr.tool_blocks.iter().all(|b| !b.started) {
            // Emit empty text block so Claude Code doesn't see an empty stream.
            yield Ok(empty_text_block_event().into_bytes());
            tr.text_open = true;
        }
        if tr.tool_blocks.iter().any(|b| b.started) {
            stop_reason = "tool_use".to_string();
        }
        for line in tr.finish(&stop_reason, input_tokens, output_tokens) {
            yield Ok::<_, std::io::Error>(line.into_bytes());
        }
        if let Some(msg) = upstream_error {
            yield Ok::<_, std::io::Error>(terminal_error_event(&msg).into_bytes());
        }
    };
    sse_stream_response(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn request_summary_counts_without_content() {
        let body = json!({
            "model": "claude-x",
            "max_tokens": 128,
            "stream": true,
            "system": "secret-system",
            "messages": [
                {"role": "user", "content": "secret-prompt"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "secret-reply"},
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "secret-path"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "secret-file-contents"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}},
                ]},
            ],
            "tools": [{"name": "Read", "description": "d", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
        });
        let s = request_summary(&body);
        let rendered = s.to_string();
        for secret in [
            "secret-prompt",
            "secret-reply",
            "secret-path",
            "secret-file-contents",
            "secret-system",
            "AAA",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        assert_eq!(s["messages"], 3);
        assert_eq!(s["tools"], json!(["Read"]));
        assert_eq!(s["max_tokens"], 128);
    }

    #[test]
    fn body_preview_truncates_at_utf8_boundary() {
        let long = format!("{}x", "é".repeat(300));
        let preview = body_preview(&long);
        assert!(preview.ends_with('…'));
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }
    #[test]
    fn body_preview_truncates() {
        assert_eq!(body_preview("  ok  "), "ok");
        let long = "x".repeat(600);
        let p = body_preview(&long);
        assert!(p.len() < 600 && p.ends_with('…'), "{p}");
    }

    // -- mock_classifier ---------------------------------------------------

    fn mock_text(resp: &Value) -> String {
        resp["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn mock_stage1_severity_only() {
        // Stage 1: system asks for `<severity>N</severity>` only; N < t1 (25)
        // means allow without ever reaching stage 2.
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [{"type": "text", "text": "Respond with <severity>N</severity> ONLY."}],
            "messages": [
                {"role": "user", "content": "some action context"},
                {"role": "user", "content": "more context blocks"},
            ],
            "tools": [],
            "tool_choice": null,
        });
        let out = mock_check_response(&body, "claude-sonnet-5").expect("stage1 mocked");
        assert_eq!(mock_text(&out), "<severity>0</severity>");
        assert_eq!(out["model"], "claude-sonnet-5");
        assert_eq!(out["type"], "message");
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn mock_stage2_block_only() {
        // Stage 2: system carries the Output Format block rules; the reply
        // must begin with `<block>`.
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [{"type": "text", "text": "## Output Format\nIf the action should be blocked:\n<block>yes</block>... If allowed: <block>no</block>"}],
            "messages": [{"role": "user", "content": "context"}],
            "tools": [],
        });
        let out = mock_check_response(&body, "claude-sonnet-5").expect("stage2 mocked");
        assert_eq!(mock_text(&out), "<block>no</block>");
    }

    #[test]
    fn mock_both_tags_replies_block_first() {
        let body = json!({
            "model": "m",
            "max_tokens": 64,
            "system": "mentions <severity>N</severity> and instructs <block>yes</block>",
            "messages": [{"role": "user", "content": "c"}],
        });
        let out = mock_check_response(&body, "m").expect("mocked");
        assert_eq!(mock_text(&out), "<block>no</block><severity>0</severity>");
    }

    #[test]
    fn mock_system_as_plain_string() {
        let body = json!({
            "model": "m",
            "system": "output: <severity>N</severity>",
            "messages": [{"role": "user", "content": "c"}],
        });
        let out = mock_check_response(&body, "m").expect("mocked");
        assert_eq!(mock_text(&out), "<severity>0</severity>");
    }

    #[test]
    fn mock_probe_reply() {
        // Observed shape: 1-char message, max_tokens 1, no system, no tools.
        let body = json!({
            "model": "claude-github-copilot-claude-sonnet-5",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "x"}],
        });
        let out = mock_check_response(&body, "claude-github-copilot-claude-sonnet-5")
            .expect("probe mocked");
        assert_eq!(mock_text(&out), "ok");
    }

    #[test]
    fn mock_ignores_real_conversation() {
        // Real agent turns carry tools — even with a classifier-like system.
        let body = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 32000,
            "system": [{"type": "text", "text": "instructions with <severity> tags"}],
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Bash", "description": "d", "input_schema": {"type": "object"}}],
        });
        assert!(mock_check_response(&body, "claude-sonnet-5").is_none());
        // tools: null / non-array must also refuse.
        let mut body = body.clone();
        body["tools"] = json!({"unexpected": true});
        assert!(mock_check_response(&body, "claude-sonnet-5").is_none());
    }

    #[test]
    fn mock_ignores_tags_quoted_in_messages() {
        // Detection reads the system only: a user pasting the tags into a
        // message must not get a canned answer.
        let body = json!({
            "model": "m",
            "max_tokens": 64,
            "system": "ordinary system prompt",
            "messages": [{"role": "user", "content": "what is <block>yes</block>?"}],
        });
        assert!(mock_check_response(&body, "m").is_none());
    }

    #[test]
    fn mock_ignores_system_without_markers() {
        let body = json!({
            "model": "m",
            "max_tokens": 64,
            "system": "You are Claude Code.",
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(mock_check_response(&body, "m").is_none());
    }

    #[test]
    fn mock_ignores_short_real_message() {
        // A short message without tools/system is not a probe unless it is
        // tiny AND max_tokens is tiny.
        let body = json!({
            "model": "m",
            "max_tokens": 32000,
            "messages": [{"role": "user", "content": "oi"}],
        });
        assert!(mock_check_response(&body, "m").is_none());
    }
}
