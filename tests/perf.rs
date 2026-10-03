//! Performance gate for the proxy's wire-protocol translation.
//!
//! Shape: a Claude simulator (the test itself, driving an in-process
//! `axum_test::TestServer` over the gateway router) → the gateway proxy → an
//! OpenCode simulator (a mock axum app on a real loopback TCP port). Each
//! sample times the full request end to end, so the measured cost is the
//! gateway's resolve + translate + forward + translate-back overhead plus one
//! real loopback round trip to the upstream. The client→gateway hop is
//! in-process (mock transport), so it is deliberately outside the metric.
//!
//! Enforcement: release builds assert `p95 <= budget` per scenario
//! (`FRANK_PERF_P95_MS` overrides every scenario at once). Debug builds run
//! and report but do not enforce — the shared `test` job runs them in debug,
//! where unoptimized timings are too noisy to gate on. Running
//! `cargo test --release --test perf` (the CI `perf` job) turns them into a
//! blocking check.
//!
//! This harness is self-contained: it mirrors a few helpers from
//! `tests/gateway.rs` (`mock_entry`/`seeded_state`) on purpose, so the perf
//! gate stays isolated from the behavior suite as it evolves.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
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
const ANTHROPIC_PKG: &str = "@opencode/ai/providers/anthropic";
const AUTH: &str = "perf-secret";
/// Explicit client session id: without it the gateway falls back to a
/// persisted `frank.session` file, putting real filesystem I/O on the hot path
/// (and this test would touch the user's home dir).
const SESSION: &str = "perf-session";
/// Release enforcement thresholds (ms). Light shapes are pure local CPU;
/// heavy shapes pay O(payload) parse/clone/serialize plus ~50 SSE chunks.
/// Overridable wholesale via `FRANK_PERF_P95_MS`.
const LIGHT_BUDGET_MS: f64 = 15.0;
const HEAVY_BUDGET_MS: f64 = 25.0;
/// Always-on sanity gauge: a non-streaming loopback request that exceeds this
/// is hung, regardless of profile.
const HANG_CEILING_MS: f64 = 2_000.0;
/// How many canned SSE chunks the simulator emits per streaming sample: enough
/// that per-chunk translator work shows up in the metric.
const STREAM_CHUNKS: usize = 50;
/// Base64 payload (~512 KiB of text) for the image scenarios: exercises the
/// data-URL string clone in the translators.
const IMAGE_B64_LEN: usize = 512 * 1024;

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
// OpenCode simulator: canned JSON / SSE on a real loopback listener.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct OpencodeSimulator {
    chat_calls: Arc<AtomicUsize>,
    responses_calls: Arc<AtomicUsize>,
    messages_calls: Arc<AtomicUsize>,
    count_calls: Arc<AtomicUsize>,
}

fn sse_response(body: String) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn sim_chat(State(s): State<OpencodeSimulator>, Json(body): Json<Value>) -> Response {
    s.chat_calls.fetch_add(1, Ordering::Relaxed);
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut sse = String::new();
        for i in 0..STREAM_CHUNKS {
            sse.push_str(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"tok{i} \"}}}}]}}\n\n"
            ));
        }
        sse.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":64,\"completion_tokens\":60}}\n\n",
        );
        sse.push_str("data: [DONE]\n\n");
        return sse_response(sse);
    }
    Json(json!({
        "id": "chatcmpl-perf",
        "choices": [{"finish_reason": "stop", "message": {"content": "perf reply"}}],
        "usage": {"prompt_tokens": 64, "completion_tokens": 8}
    }))
    .into_response()
}

async fn sim_responses(State(s): State<OpencodeSimulator>, Json(body): Json<Value>) -> Response {
    s.responses_calls.fetch_add(1, Ordering::Relaxed);
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let mut sse = String::new();
        for i in 0..STREAM_CHUNKS {
            sse.push_str(&format!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"tok{i} \"}}\n\n"
            ));
        }
        sse.push_str(
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":64,\"output_tokens\":60}}}\n\n",
        );
        return sse_response(sse);
    }
    Json(json!({
        "id": "resp-perf",
        "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "perf reply"}]}],
        "usage": {"input_tokens": 64, "output_tokens": 8}
    }))
    .into_response()
}

async fn sim_messages(State(s): State<OpencodeSimulator>, Json(body): Json<Value>) -> Response {
    s.messages_calls.fetch_add(1, Ordering::Relaxed);
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        // Anthropic passthrough streaming is byte passthrough on the gateway,
        // so the simulator speaks client-shaped SSE directly.
        let mut sse = String::from(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sim\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"mock-perf-anth\",\"stop_reason\":null}}\n\n",
        );
        for i in 0..STREAM_CHUNKS {
            sse.push_str(&format!(
                "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"tok{i} \"}}}}\n\n"
            ));
        }
        sse.push_str(
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":64,\"output_tokens\":60}}\n\n",
        );
        sse.push_str("data: {\"type\":\"message_stop\"}\n\n");
        return sse_response(sse);
    }
    Json(json!({
        "id": "msg-perf",
        "type": "message",
        "role": "assistant",
        "model": "mock-perf-anth",
        "content": [{"type": "text", "text": "mock anthropic reply"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 64, "output_tokens": 8}
    }))
    .into_response()
}

async fn sim_count_tokens(State(s): State<OpencodeSimulator>, Json(_): Json<Value>) -> Response {
    s.count_calls.fetch_add(1, Ordering::Relaxed);
    Json(json!({"input_tokens": 123})).into_response()
}

async fn spawn_sim(sim: OpencodeSimulator) -> String {
    let app = Router::new()
        .route("/chat/completions", post(sim_chat))
        .route("/responses", post(sim_responses))
        .route("/messages", post(sim_messages))
        .route("/messages/count_tokens", post(sim_count_tokens))
        .layer(axum::extract::DefaultBodyLimit::disable())
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

// ---------------------------------------------------------------------------
// Request shapes: every input class the translators accept.
// ---------------------------------------------------------------------------

/// Floor: resolve + forward with almost no translation work.
fn shape_minimal(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
    })
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

/// Image input: a large base64 block plus a tool_result carrying a nested
/// image. Exercises the data-URL string clone in the translators.
fn shape_image(model: &str) -> Value {
    let big = "QUJD".repeat(IMAGE_B64_LEN / 4);
    json!({
        "model": model,
        "max_tokens": 256,
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "describe this image"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
            ]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_img", "name": "Read", "input": {"file_path": "/img.png"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_img", "content": [
                    {"type": "text", "text": "file bytes"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}
                ]}
            ]}
        ],
        "tools": [
            {"name": "Read", "description": "Read a file",
             "input_schema": {"type": "object",
                              "properties": {"file_path": {"type": "string"}},
                              "required": ["file_path"]}}
        ]
    })
}

/// Giant text (~1 MiB): exercises the O(payload) parse → translate →
/// re-serialize triple pass on translated protocols.
fn shape_giant(model: &str) -> Value {
    let sys = "S".repeat(512 * 1024);
    let user = "U".repeat(512 * 1024);
    json!({
        "model": model,
        "max_tokens": 256,
        "system": sys,
        "messages": [{"role": "user", "content": user}]
    })
}

/// Heavy tool use: 30 tools with non-trivial schemas, `tool_choice: any`,
/// and three tool_use/tool_result pairs.
fn shape_tools_heavy(model: &str) -> Value {
    let tools: Vec<Value> = (0..30)
        .map(|i| {
            json!({
                "name": format!("Tool{i}"),
                "description": format!("Does thing {i}. ").repeat(10),
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "pattern": {"type": "string"},
                        "limit": {"type": "integer"},
                        "extra": {"type": "string", "description": "extra context ".repeat(5).to_string()}
                    },
                    "required": ["path"]
                }
            })
        })
        .collect();
    json!({
        "model": model,
        "max_tokens": 256,
        "messages": [
            {"role": "user", "content": "run the tools"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_a", "name": "Tool0", "input": {"path": "/a"}},
                {"type": "tool_use", "id": "tu_b", "name": "Tool1", "input": {"path": "/b"}},
                {"type": "tool_use", "id": "tu_c", "name": "Tool2", "input": {"path": "/c"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_a", "content": "ok a"},
                {"type": "tool_result", "tool_use_id": "tu_b", "content": "ok b"},
                {"type": "tool_result", "tool_use_id": "tu_c", "content": "ok c", "is_error": true}
            ]}
        ],
        "tools": tools,
        "tool_choice": {"type": "any"}
    })
}

/// Long conversation: ~60 alternating text turns (~50 KB). Exercises
/// per-block translation cost with many small blocks.
fn shape_multiturn(model: &str) -> Value {
    let messages: Vec<Value> = (0..60)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            json!({"role": role, "content": format!("Turn {i}: ").to_string() + &"padding ".repeat(100)})
        })
        .collect();
    json!({
        "model": model,
        "max_tokens": 256,
        "messages": messages
    })
}

/// Everything at once: medium image + tools + tool_result image + `thinking`
/// and `document` blocks (the translators drop those — this measures the
/// cost of the discard path, which still parses and walks them).
fn shape_kitchen_sink(model: &str) -> Value {
    let img = "QUJD".repeat(32 * 1024);
    let tools: Vec<Value> = (0..5)
        .map(|i| {
            json!({
                "name": format!("KTool{i}"),
                "description": "kitchen tool",
                "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}}
            })
        })
        .collect();
    json!({
        "model": model,
        "max_tokens": 256,
        "system": "kitchen sink",
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "all at once"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": img}},
                {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "QUJD"}}
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "tool_use", "id": "tu_k", "name": "KTool0", "input": {"q": "x"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_k", "content": [
                    {"type": "text", "text": "done"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}
                ]}
            ]}
        ],
        "tools": tools,
        "tool_choice": {"type": "any"}
    })
}

/// Streaming variant of the realistic payload. Metric is the total time until
/// the client-visible Anthropic SSE is fully drained.
fn shape_stream(model: &str) -> Value {
    let mut v = realistic_body(model);
    v["stream"] = Value::Bool(true);
    v
}

// ---------------------------------------------------------------------------
// Scenario table + measurement harness.
// ---------------------------------------------------------------------------

/// What the harness asserts on sample 0 (every sample must still be HTTP 200).
#[derive(Clone, Copy)]
enum Expect {
    /// Non-streaming JSON: `content[0].text` equals this.
    JsonText(&'static str),
    /// Streaming SSE: drained text contains this marker.
    Sse(&'static str),
    /// `count_tokens`: exact `input_tokens` (proxy path).
    CountExact(u64),
    /// `count_tokens`: any positive `input_tokens` (local estimate path).
    CountAny,
}

#[derive(Clone, Copy, PartialEq)]
enum Counter {
    Chat,
    Responses,
    Messages,
    Count,
    /// No upstream call expected (local `estimate_tokens` path).
    None,
}

struct Scenario {
    /// `[perf]` label. CI regex constraint: `[a-z_]+` only (lowercase,
    /// underscores — no digits, hyphens or uppercase), or the row silently
    /// vanishes from the summary table.
    label: &'static str,
    alias: &'static str,
    endpoint: &'static str,
    body: fn(&str) -> Value,
    expect: Expect,
    counter: Counter,
    heavy: bool,
    /// Release budget for this scenario; `FRANK_PERF_P95_MS` overrides all.
    budget_ms: f64,
}

fn sample_counts(heavy: bool) -> (usize, usize) {
    if cfg!(debug_assertions) {
        if heavy {
            (2, 8)
        } else {
            (3, 15)
        }
    } else if heavy {
        (10, 100)
    } else {
        (20, 200)
    }
}

fn budget_for(scenario_default: f64) -> Option<f64> {
    match std::env::var("FRANK_PERF_P95_MS") {
        // Explicit override enforces in any profile.
        Ok(v) => v.parse::<f64>().ok(),
        Err(_) if cfg!(debug_assertions) => None,
        Err(_) => Some(scenario_default),
    }
}

fn counter_value(sim: &OpencodeSimulator, c: Counter) -> usize {
    match c {
        Counter::Chat => sim.chat_calls.load(Ordering::Relaxed),
        Counter::Responses => sim.responses_calls.load(Ordering::Relaxed),
        Counter::Messages => sim.messages_calls.load(Ordering::Relaxed),
        Counter::Count => sim.count_calls.load(Ordering::Relaxed),
        Counter::None => 0,
    }
}

/// Fire `warmup + samples` requests, timing each. The body is pre-serialized
/// once (`.json()` would re-serialize inside the timed region) and sent as
/// refcounted `Bytes`. The first request also validates the response shape, so
/// the measured path is proven to be the real translation path.
async fn run_samples(
    server: &axum_test::TestServer,
    sc: &Scenario,
    warmup: usize,
    samples: usize,
) -> Vec<f64> {
    let body =
        axum::body::Bytes::from(serde_json::to_vec(&(sc.body)(sc.alias)).expect("serialize body"));
    let total = warmup + samples;
    let mut lat = Vec::with_capacity(samples);
    for i in 0..total {
        let t = Instant::now();
        let resp = server
            .post(sc.endpoint)
            .add_header("x-api-key", AUTH)
            .add_header("x-claude-code-session-id", SESSION)
            .content_type("application/json")
            .bytes(body.clone())
            .await;
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(resp.status_code(), 200, "{}: sample {i}", sc.label);
        if i == 0 {
            match sc.expect {
                Expect::JsonText(want) => {
                    let v: Value = resp.json();
                    assert_eq!(v["content"][0]["text"], want, "{}", sc.label);
                }
                Expect::Sse(marker) => {
                    let text = resp.text();
                    assert!(text.contains(marker), "{}: {text}", sc.label);
                }
                Expect::CountExact(want) => {
                    let v: Value = resp.json();
                    assert_eq!(v["input_tokens"], want, "{}", sc.label);
                }
                Expect::CountAny => {
                    let v: Value = resp.json();
                    assert!(v["input_tokens"].as_u64().unwrap_or(0) > 0, "{}", sc.label);
                }
            }
        }
        if i >= warmup {
            lat.push(ms);
        }
    }
    lat
}

/// Print the `[perf]` record (layout is parsed by the CI summary — keep it
/// byte-identical) and check the budgets. Returns `Err(p95)` on any violation
/// instead of asserting, so callers can print every scenario before failing.
fn report(
    label: &str,
    stats: &Percentiles,
    warmup: usize,
    budget_ms: Option<f64>,
) -> Result<(), f64> {
    eprintln!(
        "[perf] {label}: n={} warmup={} p50={:.3}ms p95={:.3}ms mean={:.3}ms min={:.3}ms max={:.3}ms",
        stats.sorted_ms.len(),
        warmup,
        stats.p50().unwrap_or(f64::NAN),
        stats.p95().unwrap_or(f64::NAN),
        stats.mean().unwrap_or(f64::NAN),
        stats.min().unwrap_or(f64::NAN),
        stats.max().unwrap_or(f64::NAN),
    );
    let p95 = stats.p95().expect("non-empty samples");
    if p95 > HANG_CEILING_MS {
        return Err(p95);
    }
    match budget_ms {
        Some(budget) if p95 > budget => Err(p95),
        Some(_) => Ok(()),
        None => {
            eprintln!(
                "[perf] {label}: debug build, budget not enforced (set FRANK_PERF_P95_MS to enforce)"
            );
            Ok(())
        }
    }
}

/// Run a family of scenarios against one server, printing every `[perf]` line
/// before failing. Upstream call deltas prove each scenario actually ran the
/// forward path (except `Counter::None`, the local-estimate path).
async fn run_family(
    server: &axum_test::TestServer,
    sim: &OpencodeSimulator,
    scenarios: &[Scenario],
) -> Vec<String> {
    let mut failures = Vec::new();
    for sc in scenarios {
        let (warmup, samples) = sample_counts(sc.heavy);
        let budget = budget_for(sc.budget_ms);
        let before = counter_value(sim, sc.counter);
        let stats = Percentiles::new(run_samples(server, sc, warmup, samples).await);
        if report(sc.label, &stats, warmup, budget).is_err() {
            let p95 = stats.p95().unwrap_or(f64::NAN);
            failures.push(format!(
                "{}: p95 {p95:.3}ms exceeds budget {}",
                sc.label,
                budget.map_or("hang-ceiling".to_string(), |b| format!("{b:.1}ms"))
            ));
        }
        if sc.counter != Counter::None {
            let got = counter_value(sim, sc.counter) - before;
            assert_eq!(
                got,
                warmup + samples,
                "{}: upstream call count mismatch",
                sc.label
            );
        }
    }
    failures
}

const CHAT_ALIAS: &str = "claude-perf-chat";
const RESP_ALIAS: &str = "claude-perf-resp";
const ANTH_ALIAS: &str = "claude-perf-anth";
const MSGS: &str = "/v1/messages";
const COUNT: &str = "/v1/messages/count_tokens";

#[tokio::test]
async fn perf_chat_matrix() {
    let sim = OpencodeSimulator::default();
    let base = spawn_sim(sim.clone()).await;
    let state = perf_state(
        vec![perf_entry(&base, CHAT_PKG, "mock-perf-chat")],
        vec![alias(CHAT_ALIAS, "opencode/mock-perf-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let scenarios = [
        Scenario {
            label: "chat_minimal",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_minimal,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
        Scenario {
            label: "chat_realistic",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: realistic_body,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
        Scenario {
            label: "chat_image",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_image,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "chat_giant",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_giant,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "chat_tools_heavy",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_tools_heavy,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "chat_multiturn",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_multiturn,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "chat_kitchen_sink",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_kitchen_sink,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "chat_stream",
            alias: CHAT_ALIAS,
            endpoint: MSGS,
            body: shape_stream,
            expect: Expect::Sse("message_stop"),
            counter: Counter::Chat,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
    ];
    let failures = run_family(&server, &sim, &scenarios).await;
    assert!(
        failures.is_empty(),
        "perf budget failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn perf_responses_matrix() {
    let sim = OpencodeSimulator::default();
    let base = spawn_sim(sim.clone()).await;
    let state = perf_state(
        vec![perf_entry(&base, RESPONSES_PKG, "mock-perf-resp")],
        vec![alias(RESP_ALIAS, "opencode/mock-perf-resp")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let scenarios = [
        Scenario {
            label: "responses_minimal",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_minimal,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
        Scenario {
            label: "responses_realistic",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: realistic_body,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
        Scenario {
            label: "responses_image",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_image,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "responses_giant",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_giant,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "responses_tools_heavy",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_tools_heavy,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "responses_kitchen_sink",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_kitchen_sink,
            expect: Expect::JsonText("perf reply"),
            counter: Counter::Responses,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        Scenario {
            label: "responses_stream",
            alias: RESP_ALIAS,
            endpoint: MSGS,
            body: shape_stream,
            expect: Expect::Sse("message_stop"),
            counter: Counter::Responses,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
    ];
    let failures = run_family(&server, &sim, &scenarios).await;
    assert!(
        failures.is_empty(),
        "perf budget failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn perf_passthrough_and_count_tokens() {
    let sim = OpencodeSimulator::default();
    let base = spawn_sim(sim.clone()).await;
    let state = perf_state(
        vec![
            perf_entry(&base, ANTHROPIC_PKG, "mock-perf-anth"),
            perf_entry(&base, CHAT_PKG, "mock-perf-chat"),
        ],
        vec![
            alias(ANTH_ALIAS, "opencode/mock-perf-anth"),
            alias(CHAT_ALIAS, "opencode/mock-perf-chat"),
        ],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let scenarios = [
        Scenario {
            label: "passthrough_json",
            alias: ANTH_ALIAS,
            endpoint: MSGS,
            body: realistic_body,
            expect: Expect::JsonText("mock anthropic reply"),
            counter: Counter::Messages,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
        Scenario {
            label: "anthropic_stream",
            alias: ANTH_ALIAS,
            endpoint: MSGS,
            body: shape_stream,
            expect: Expect::Sse("message_stop"),
            counter: Counter::Messages,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        // Local estimate path: no upstream call (giant body stresses the walk).
        Scenario {
            label: "count_tokens_local",
            alias: CHAT_ALIAS,
            endpoint: COUNT,
            body: shape_giant,
            expect: Expect::CountAny,
            counter: Counter::None,
            heavy: true,
            budget_ms: HEAVY_BUDGET_MS,
        },
        // Proxy path: the simulator answers the count.
        Scenario {
            label: "count_tokens_proxy",
            alias: ANTH_ALIAS,
            endpoint: COUNT,
            body: realistic_body,
            expect: Expect::CountExact(123),
            counter: Counter::Count,
            heavy: false,
            budget_ms: LIGHT_BUDGET_MS,
        },
    ];
    let failures = run_family(&server, &sim, &scenarios).await;
    assert!(
        failures.is_empty(),
        "perf budget failures:\n{}",
        failures.join("\n")
    );
}
