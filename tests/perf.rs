//! Performance gate for the proxy's wire-protocol translation.
//!
//! Shape: a Claude simulator (the test itself, driving an in-process
//! `axum_test::TestServer` over the gateway router) → the gateway proxy → an
//! OpenCode simulator (a mock axum app on a real loopback TCP port). Each
//! sample times `POST /v1/messages` end to end, so the measured cost is the
//! gateway's resolve + translate + forward + translate-back overhead plus one
//! real loopback round trip to the upstream. The client→gateway hop is
//! in-process (mock transport), so it is deliberately outside the metric.
//!
//! Enforcement: release builds assert `p95 <= FRANK_PERF_P95_MS` (default 15).
//! Debug builds run and report but do not enforce — the shared `test` job runs
//! them in debug, where unoptimized timings are too noisy to gate on. Running
//! `cargo test --release --test perf` (the CI `perf` job) turns them into a
//! blocking check. Set `FRANK_PERF_P95_MS` to enforce in any profile.
//!
//! This harness is self-contained: it mirrors a few helpers from
//! `tests/gateway.rs` (`mock_entry`/`seeded_state`) on purpose, so the perf
//! gate stays isolated from the behavior suite as it evolves.

use axum::{extract::State, routing::post, Json, Router};
use frank_opencode::api::server::{router, AppState};
use frank_opencode::config::AppConfig;
use frank_opencode::domain::{AliasEntry, CatalogEntry, CatalogSettings};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Instant;

const CHAT_PKG: &str = "@opencode/ai/providers/openai-compatible";
const RESPONSES_PKG: &str = "@opencode/ai/providers/openai";
const AUTH: &str = "perf-secret";
/// Explicit client session id: without it the gateway falls back to a
/// persisted `frank.session` file, putting real filesystem I/O on the hot path
/// (and this test would touch the user's home dir).
const SESSION: &str = "perf-session";
/// Release enforcement threshold (ms). Overridable via `FRANK_PERF_P95_MS`.
const RELEASE_BUDGET_MS: f64 = 15.0;
/// Always-on sanity gauge: a non-streaming loopback request that exceeds this
/// is hung, regardless of profile.
const HANG_CEILING_MS: f64 = 2_000.0;

// ---------------------------------------------------------------------------
// Statistics: nearest-rank percentile, deterministic, no interpolation.
// ---------------------------------------------------------------------------

struct Percentiles {
    sorted_ms: Vec<f64>,
}

impl Percentiles {
    fn new(mut samples_ms: Vec<f64>) -> Self {
        samples_ms.sort_by(f64::total_cmp);
        Self {
            sorted_ms: samples_ms,
        }
    }

    /// Nearest-rank percentile: the smallest sample at or above the `q`
    /// quantile. Conservative and cheap; `q` is clamped to `[0, 1]`.
    fn p(&self, q: f64) -> Option<f64> {
        let n = self.sorted_ms.len();
        if n == 0 {
            return None;
        }
        let rank = (q.clamp(0.0, 1.0) * n as f64).ceil() as usize;
        Some(self.sorted_ms[rank.saturating_sub(1).min(n - 1)])
    }

    fn p50(&self) -> Option<f64> {
        self.p(0.50)
    }

    fn p95(&self) -> Option<f64> {
        self.p(0.95)
    }

    fn mean(&self) -> Option<f64> {
        if self.sorted_ms.is_empty() {
            return None;
        }
        Some(self.sorted_ms.iter().sum::<f64>() / self.sorted_ms.len() as f64)
    }

    fn min(&self) -> Option<f64> {
        self.sorted_ms.first().copied()
    }

    fn max(&self) -> Option<f64> {
        self.sorted_ms.last().copied()
    }
}

#[test]
fn percentile_empty_is_none() {
    let p = Percentiles::new(vec![]);
    assert_eq!(p.p50(), None);
    assert_eq!(p.p95(), None);
    assert_eq!(p.mean(), None);
    assert_eq!(p.min(), None);
    assert_eq!(p.max(), None);
}

#[test]
fn percentile_single_sample() {
    let p = Percentiles::new(vec![7.0]);
    assert_eq!(p.p50(), Some(7.0));
    assert_eq!(p.p95(), Some(7.0));
    assert_eq!(p.mean(), Some(7.0));
    assert_eq!(p.min(), Some(7.0));
    assert_eq!(p.max(), Some(7.0));
}

#[test]
fn percentile_nearest_rank_1_to_100() {
    let p = Percentiles::new((1..=100).map(|i| i as f64).collect());
    assert_eq!(p.p(0.0), Some(1.0));
    assert_eq!(p.p50(), Some(50.0));
    assert_eq!(p.p95(), Some(95.0));
    assert_eq!(p.p(1.0), Some(100.0));
    assert_eq!(p.min(), Some(1.0));
    assert_eq!(p.max(), Some(100.0));
    // mean of 1..=100
    assert_eq!(p.mean(), Some(50.5));
}

#[test]
fn percentile_p95_clamps_at_max_rank() {
    // n=20 → the 95th percentile is the 19th smallest, never index 20.
    let p = Percentiles::new((1..=20).map(|i| i as f64).collect());
    assert_eq!(p.p95(), Some(19.0));
}

#[test]
fn percentile_is_monotonic() {
    let p = Percentiles::new((1..=100).map(|i| i as f64).collect());
    assert!(p.p50().unwrap() <= p.p95().unwrap());
    assert!(p.p95().unwrap() <= p.max().unwrap());
}

#[test]
fn percentile_ignores_input_order() {
    let p = Percentiles::new(vec![3.0, 1.0, 2.0]);
    assert_eq!(p.p50(), Some(2.0));
    assert_eq!(p.min(), Some(1.0));
    assert_eq!(p.max(), Some(3.0));
}

// ---------------------------------------------------------------------------
// OpenCode simulator: canned non-streaming JSON on a real loopback listener.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct OpencodeSimulator {
    chat_calls: Arc<AtomicUsize>,
    responses_calls: Arc<AtomicUsize>,
}

async fn sim_chat(State(s): State<OpencodeSimulator>, Json(_): Json<Value>) -> Json<Value> {
    s.chat_calls.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "id": "chatcmpl-perf",
        "choices": [{"finish_reason": "stop", "message": {"content": "perf reply"}}],
        "usage": {"prompt_tokens": 64, "completion_tokens": 8}
    }))
}

async fn sim_responses(State(s): State<OpencodeSimulator>, Json(_): Json<Value>) -> Json<Value> {
    s.responses_calls.fetch_add(1, Ordering::Relaxed);
    Json(json!({
        "id": "resp-perf",
        "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "perf reply"}]}],
        "usage": {"input_tokens": 64, "output_tokens": 8}
    }))
}

async fn spawn_sim(sim: OpencodeSimulator) -> String {
    let app = Router::new()
        .route("/chat/completions", post(sim_chat))
        .route("/responses", post(sim_responses))
        .with_state(sim);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Seeding (mirrors `tests/gateway.rs` helpers, kept local on purpose).
// ---------------------------------------------------------------------------

fn gateway_config() -> AppConfig {
    AppConfig {
        auth_token: AUTH.to_string(),
        ..AppConfig::default()
    }
}

/// Catalog entry pointing at the simulator. Provider `opencode` with an inline
/// api_key needs no credential DB, so the forward path is exercised.
fn perf_entry(base: &str, package: &str, model: &str) -> CatalogEntry {
    CatalogEntry {
        id: model.to_string(),
        model_id: model.to_string(),
        provider_id: "opencode".to_string(),
        name: model.to_string(),
        package: package.to_string(),
        settings: CatalogSettings {
            base_url: Some(base.to_string()),
            api_key: Some("test-upstream-key".to_string()),
            provider: None,
            endpoint: None,
        },
        limit: None,
        enabled: true,
        variants: vec![],
        headers: None,
        body: None,
    }
}

fn alias(gateway: &str, opencode_ref: &str) -> AliasEntry {
    AliasEntry {
        gateway_id: gateway.to_string(),
        opencode_ref: opencode_ref.to_string(),
        display_name: gateway.to_string(),
        description: "perf".to_string(),
        context_window: None,
    }
}

async fn perf_state(entries: Vec<CatalogEntry>, aliases: Vec<AliasEntry>) -> AppState {
    let state = AppState::new(
        gateway_config(),
        PathBuf::from("/nonexistent-frank-perf/opencode.db"),
    );
    *state.catalog.write().await = entries;
    *state.aliases.write().await = aliases;
    state
}

/// Realistic Anthropic payload (~8–16 KB): a long system prompt, a
/// tool_use/tool_result pair and two tools, so translation is not trivially
/// zero. Grow `FILLER_LINES` to raise the per-request cost.
fn realistic_body(model: &str) -> Value {
    const FILLER_LINES: usize = 40;
    let filler = "context line ".repeat(FILLER_LINES);
    json!({
        "model": model,
        "max_tokens": 256,
        "system": format!("You are a coding agent.\n{filler}"),
        "messages": [
            {"role": "user", "content": "Explain the module."},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "Read",
                 "input": {"file_path": "/src/lib.rs"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": filler}]}
        ],
        "tools": [
            {"name": "Read", "description": "Read a file",
             "input_schema": {"type": "object",
                              "properties": {"file_path": {"type": "string"}},
                              "required": ["file_path"]}},
            {"name": "Grep", "description": "Search",
             "input_schema": {"type": "object",
                              "properties": {"pattern": {"type": "string"}}}}
        ]
    })
}

// ---------------------------------------------------------------------------
// Measurement harness.
// ---------------------------------------------------------------------------

struct PerfConfig {
    warmup: usize,
    samples: usize,
    budget_ms: Option<f64>,
}

fn perf_config() -> PerfConfig {
    let budget_ms = match std::env::var("FRANK_PERF_P95_MS") {
        // Explicit override enforces in any profile.
        Ok(v) => v.parse::<f64>().ok(),
        Err(_) if cfg!(debug_assertions) => None,
        Err(_) => Some(RELEASE_BUDGET_MS),
    };
    let (warmup, samples) = if cfg!(debug_assertions) {
        (5, 50)
    } else {
        (20, 200)
    };
    PerfConfig {
        warmup,
        samples,
        budget_ms,
    }
}

/// Fire `warmup + samples` requests, timing each. The body is pre-serialized
/// once (`.json()` would re-serialize inside the timed region) and sent as
/// refcounted `Bytes`. The first request also validates the response shape, so
/// the measured path is proven to be the real translation path.
async fn run_samples(
    server: &axum_test::TestServer,
    body: axum::body::Bytes,
    label: &str,
    cfg: &PerfConfig,
) -> Vec<f64> {
    let total = cfg.warmup + cfg.samples;
    let mut lat = Vec::with_capacity(cfg.samples);
    for i in 0..total {
        let t = Instant::now();
        let resp = server
            .post("/v1/messages")
            .add_header("x-api-key", AUTH)
            .add_header("x-claude-code-session-id", SESSION)
            .content_type("application/json")
            .bytes(body.clone())
            .await;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(resp.status_code(), 200, "{label}: sample {i}");
        if i == 0 {
            let v: Value = resp.json();
            assert_eq!(v["content"][0]["text"], "perf reply", "{label}");
        }
        if i >= cfg.warmup {
            lat.push(ms);
        }
    }
    lat
}

fn report(label: &str, stats: &Percentiles, cfg: &PerfConfig) {
    eprintln!(
        "[perf] {label}: n={} warmup={} p50={:.3}ms p95={:.3}ms mean={:.3}ms min={:.3}ms max={:.3}ms",
        stats.sorted_ms.len(),
        cfg.warmup,
        stats.p50().unwrap_or(f64::NAN),
        stats.p95().unwrap_or(f64::NAN),
        stats.mean().unwrap_or(f64::NAN),
        stats.min().unwrap_or(f64::NAN),
        stats.max().unwrap_or(f64::NAN),
    );
    let p95 = stats.p95().expect("non-empty samples");
    assert!(
        p95 <= HANG_CEILING_MS,
        "{label}: p95 {p95:.3}ms exceeds the {HANG_CEILING_MS:.0}ms hang ceiling"
    );
    match cfg.budget_ms {
        Some(budget) => assert!(
            p95 <= budget,
            "{label}: p95 {p95:.3}ms exceeds budget {budget:.3}ms"
        ),
        None => eprintln!(
            "[perf] {label}: debug build, budget not enforced (set FRANK_PERF_P95_MS to enforce)"
        ),
    }
}

#[tokio::test]
async fn perf_chat_completions_translation_p95() {
    let sim = OpencodeSimulator::default();
    let base = spawn_sim(sim.clone()).await;
    let state = perf_state(
        vec![perf_entry(&base, CHAT_PKG, "mock-perf-chat")],
        vec![alias("claude-perf-chat", "opencode/mock-perf-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let body = axum::body::Bytes::from(
        serde_json::to_vec(&realistic_body("claude-perf-chat")).expect("serialize body"),
    );

    let cfg = perf_config();
    let stats = Percentiles::new(run_samples(&server, body, "chat_completions", &cfg).await);
    report("chat_completions", &stats, &cfg);
    // Every sample (warmup included) must have reached the upstream: proves the
    // translation + forward path actually ran.
    assert_eq!(
        sim.chat_calls.load(Ordering::Relaxed),
        cfg.warmup + cfg.samples,
        "upstream call count mismatch"
    );
}

#[tokio::test]
async fn perf_responses_translation_p95() {
    let sim = OpencodeSimulator::default();
    let base = spawn_sim(sim.clone()).await;
    let state = perf_state(
        vec![perf_entry(&base, RESPONSES_PKG, "mock-perf-resp")],
        vec![alias("claude-perf-resp", "opencode/mock-perf-resp")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let body = axum::body::Bytes::from(
        serde_json::to_vec(&realistic_body("claude-perf-resp")).expect("serialize body"),
    );

    let cfg = perf_config();
    let stats = Percentiles::new(run_samples(&server, body, "responses", &cfg).await);
    report("responses", &stats, &cfg);
    assert_eq!(
        sim.responses_calls.load(Ordering::Relaxed),
        cfg.warmup + cfg.samples,
        "upstream call count mismatch"
    );
}
