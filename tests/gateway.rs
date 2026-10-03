//! Integration tests against a seeded gateway. The boot-retry tests use a
//! fake `opencode` shim binary; everything else needs no real binary.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use frank_opencode::api::server::{router, AppState};
use frank_opencode::config::AppConfig;
use frank_opencode::domain::{AliasEntry, CatalogEntry, CatalogLimit, CatalogSettings};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

fn test_config() -> AppConfig {
    AppConfig {
        auth_token: "test-secret".to_string(),
        ..AppConfig::default()
    }
}

fn entry(provider: &str, model: &str) -> CatalogEntry {
    CatalogEntry {
        id: model.to_string(),
        model_id: model.to_string(),
        provider_id: provider.to_string(),
        name: model.to_string(),
        package: "@opencode/ai/providers/openai-compatible".to_string(),
        settings: CatalogSettings {
            base_url: Some("http://127.0.0.1:9".to_string()),
            api_key: None,
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
        description: "test".to_string(),
        context_window: None,
        family_tier: None,
        family_default: false,
    }
}

async fn seeded_state(
    config: AppConfig,
    entries: Vec<CatalogEntry>,
    aliases: Vec<AliasEntry>,
) -> AppState {
    let state = AppState::new(config, PathBuf::from("/nonexistent-frank-test/opencode.db"));
    *state.catalog.write().await = entries;
    *state.aliases.write().await = aliases;
    state
}

fn msg_body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

#[tokio::test]
async fn health_is_open_without_token() {
    let server =
        axum_test::TestServer::new(router(seeded_state(test_config(), vec![], vec![]).await))
            .unwrap();
    let resp = server.get("/health").await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["status"], "starting");
}

#[tokio::test]
async fn models_expose_context_window_when_catalog_knows_it() {
    // `limit.context` comes from `opencode api get /api/model`, same shape the
    // catalog reports (also exercised by the domain test `context_window_from_catalog_limit`).
    let mut e = entry("opencode-go", "deepseek-v4-flash");
    e.limit = Some(CatalogLimit {
        context: Some(1_000_000),
        input: Some(900_000),
        output: Some(128_000),
    });
    // Seeded state skips `refresh()`, so propagate the window as it would.
    let mut a = alias("claude-x", "opencode-go/deepseek-v4-flash");
    a.context_window = e.context_window();
    let state = seeded_state(test_config(), vec![e], vec![a]).await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let data: Value = server
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    let item = &data["data"][0];
    // Known window → announced via the `[Nm]` suffix on the id (what mainline
    // Claude Code reads) plus the structured `context_window` field.
    assert_eq!(item["id"], "claude-x[1m]");
    assert_eq!(item["context_window"], 1_000_000);

    // Without a catalog `limit`, the field is omitted (not null): clients
    // then fall back to their default window.
    let server = axum_test::TestServer::new(router(
        seeded_state(
            test_config(),
            vec![entry("opencode-go", "kimi-k2.7-code")],
            vec![alias("claude-y", "opencode-go/kimi-k2.7-code")],
        )
        .await,
    ))
    .unwrap();
    let data: Value = server
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    let item = &data["data"][0];
    assert_eq!(item["id"], "claude-y");
    assert!(item.get("context_window").is_none(), "{item}");

    // A window below 1M still announces `context_window` but no `[1m]`
    // suffix: mainline Claude Code would only read the 1M literal.
    let mut e = entry("opencode-go", "qwen3-8-max");
    e.limit = Some(CatalogLimit {
        context: Some(128_000),
        input: None,
        output: None,
    });
    let mut a = alias("claude-z", "opencode-go/qwen3-8-max");
    a.context_window = e.context_window();
    let server =
        axum_test::TestServer::new(router(seeded_state(test_config(), vec![e], vec![a]).await))
            .unwrap();
    let data: Value = server
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    let item = &data["data"][0];
    assert_eq!(item["id"], "claude-z");
    assert_eq!(item["context_window"], 128_000);
}

#[tokio::test]
async fn models_announce_anthropic_family_tier_only_when_mapped() {
    // A tiered alias emits `anthropic_family_tier` (and `is_family_default`
    // when flagged); unmapped aliases omit both so Desktop keeps its current
    // substring fallback.
    let mut fast = alias("claude-spark", "opencode-go/muse-spark");
    fast.family_tier = Some("haiku".to_string());
    fast.family_default = true;
    let plain = alias("claude-other", "other/model");
    let state = seeded_state(test_config(), vec![], vec![plain, fast]).await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let data: Value = server
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    // Aliases sort by gateway_id: claude-other first, claude-spark second.
    let other = &data["data"][0];
    assert!(other.get("anthropic_family_tier").is_none(), "{other}");
    assert!(other.get("is_family_default").is_none(), "{other}");
    let spark = &data["data"][1];
    assert_eq!(spark["anthropic_family_tier"], "haiku");
    assert_eq!(spark["is_family_default"], true);

    // Tier without the default flag omits `is_family_default` (the Desktop
    // only honors the flag together with a tier — same rule as its own
    // `inferenceModels` entries).
    let mut sonnet = alias("claude-copilot", "github-copilot/claude-sonnet-5");
    sonnet.family_tier = Some("sonnet".to_string());
    let state = seeded_state(test_config(), vec![], vec![sonnet]).await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let data: Value = server
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    let item = &data["data"][0];
    assert_eq!(item["anthropic_family_tier"], "sonnet");
    assert!(item.get("is_family_default").is_none(), "{item}");
}

#[tokio::test]
async fn auth_middleware_rejects_bad_token() {
    let server =
        axum_test::TestServer::new(router(seeded_state(test_config(), vec![], vec![]).await))
            .unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "wrong")
        .json(&msg_body("whatever"))
        .await;
    assert_eq!(resp.status_code(), 401);
    let body: Value = resp.json();
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn auth_accepts_either_credential_header() {
    let server =
        axum_test::TestServer::new(router(seeded_state(test_config(), vec![], vec![]).await))
            .unwrap();
    // Empty catalog -> 404 proves the request passed the middleware.
    for (name, value) in [
        ("x-api-key", "test-secret"),
        ("authorization", "Bearer test-secret"),
    ] {
        let resp = server
            .post("/v1/messages")
            .add_header(name, value)
            .json(&msg_body("whatever"))
            .await;
        assert_eq!(resp.status_code(), 404, "header {name}");
    }
}

#[tokio::test]
async fn auth_disabled_when_no_token_configured() {
    let cfg = AppConfig {
        auth_token: String::new(),
        ..AppConfig::default()
    };
    let server =
        axum_test::TestServer::new(router(seeded_state(cfg, vec![], vec![]).await)).unwrap();
    let resp = server
        .post("/v1/messages")
        .json(&msg_body("whatever"))
        .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn window_suffix_still_resolves_alias() {
    let state = seeded_state(
        test_config(),
        vec![entry("opencode-go", "kimi-k2.7-code")],
        vec![alias("claude-x", "opencode-go/kimi-k2.7-code")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    // Resolves (no credential in test DB) -> 401 proves it is NOT a 404.
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-x[1m]"))
        .await;
    assert_eq!(resp.status_code(), 401);
    let body: Value = resp.json();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("no stored credential"),
        "{body}"
    );
}

#[tokio::test]
async fn ambiguous_model_id_resolves_deterministically() {
    let state = seeded_state(
        test_config(),
        vec![entry("b-provider", "dup"), entry("a-provider", "dup")],
        vec![],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let first: Value = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("dup"))
        .await
        .json();
    let second: Value = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("dup"))
        .await
        .json();
    assert_eq!(first["error"]["message"], second["error"]["message"]);
    assert!(
        first["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("a-provider"),
        "{first}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end with a mocked upstream: real HTTP on 127.0.0.1, no `opencode`
// binary, no network. Exercises request translation, forwarding, response
// translation and session headers for all three wire protocols.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockUpstream {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    sessions: Arc<Mutex<Vec<String>>>,
    betas: Arc<Mutex<Vec<Option<String>>>>,
    org_ids: Arc<Mutex<Vec<Option<String>>>>,
    chat_streaming: bool,
    /// Emit a streaming Chat response that ends after a partial frame,
    /// mid-SSE. Exercises the stream-failure path.
    chat_stream_truncated: bool,
}

async fn note(st: &MockUpstream, headers: &axum::http::HeaderMap, path: &str, body: Value) {
    st.calls.lock().await.push((path.to_string(), body));
    if let Some(s) = headers
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
    {
        st.sessions.lock().await.push(s.to_string());
    }
    st.betas.lock().await.push(
        headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    );
    st.org_ids.lock().await.push(
        headers
            .get("x-opencode-org-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    );
}

async fn mock_chat(
    State(st): State<MockUpstream>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    note(&st, &headers, "chat/completions", body).await;
    if st.chat_stream_truncated {
        // A partial SSE frame, a pause so the response headers flush, then a
        // body error: the gateway must surface an error event instead of
        // ending 200 with no explanation.
        let s = async_stream::stream! {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            ));
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            yield Err(std::io::Error::other("upstream dropped"));
        };
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from_stream(s))
            .unwrap();
    }
    if st.chat_streaming {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(axum::body::Body::from(sse))
            .unwrap();
    }
    Json(json!({
        "id": "chatcmpl-test",
        "choices": [{"finish_reason": "stop", "message": {"content": "mock chat reply"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5}
    }))
    .into_response()
}

async fn mock_responses(
    State(st): State<MockUpstream>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    note(&st, &headers, "responses", body).await;
    Json(json!({
        "id": "resp-test",
        "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": "mock responses reply"}]}],
        "usage": {"input_tokens": 3, "output_tokens": 5}
    }))
    .into_response()
}

async fn mock_messages(
    State(st): State<MockUpstream>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let model = body.get("model").cloned().unwrap_or(Value::Null);
    note(&st, &headers, "messages", body).await;
    Json(json!({
        "id": "msg_upstream",
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": "mock anthropic reply"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 2}
    }))
    .into_response()
}

async fn mock_count_tokens(
    State(st): State<MockUpstream>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    note(&st, &headers, "messages/count_tokens", body).await;
    Json(json!({"input_tokens": 7})).into_response()
}

async fn spawn_mock(mock: MockUpstream) -> String {
    let app = Router::new()
        .route("/chat/completions", post(mock_chat))
        .route("/responses", post(mock_responses))
        .route("/messages/count_tokens", post(mock_count_tokens))
        .route("/messages", post(mock_messages))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Catalog entry pointing at the mock upstream. Provider `opencode` with an
/// inline api_key needs no credential DB, so the forward path is exercised.
fn mock_entry(base_url: &str, package: &str, model: &str) -> CatalogEntry {
    CatalogEntry {
        id: model.to_string(),
        model_id: model.to_string(),
        provider_id: "opencode".to_string(),
        name: model.to_string(),
        package: package.to_string(),
        settings: CatalogSettings {
            base_url: Some(base_url.to_string()),
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

async fn calls_to(mock: &MockUpstream, path: &str) -> Vec<Value> {
    mock.calls
        .lock()
        .await
        .iter()
        .filter(|(p, _)| p == path)
        .map(|(_, b)| b.clone())
        .collect()
}

fn msg_body_tools(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "Read", "description": "d", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "none"}
    })
}

const CHAT_PKG: &str = "@opencode/ai/providers/openai-compatible";
const RESPONSES_PKG: &str = "@opencode/ai/providers/openai";
const ANTHROPIC_PKG: &str = "@opencode/ai/providers/anthropic";

#[tokio::test]
async fn e2e_chat_completions_round_trip() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, CHAT_PKG, "mock-chat")],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-chat"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["model"], "claude-mock-chat");
    assert_eq!(body["content"][0]["text"], "mock chat reply");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 3);
    assert_eq!(body["usage"]["output_tokens"], 5);

    // Upstream saw the translated request with its own model id.
    let calls = calls_to(&mock, "chat/completions").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], "mock-chat");
    assert_eq!(calls[0]["messages"][0]["content"], "hi");
    // The Go routing header is always sent (fallback session id here).
    let sessions = mock.sessions.lock().await;
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].starts_with("frank-"), "{}", sessions[0]);
}

#[tokio::test]
async fn e2e_responses_round_trip() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, RESPONSES_PKG, "mock-resp")],
        vec![alias("claude-mock-resp", "opencode/mock-resp")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-resp"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["model"], "claude-mock-resp");
    assert_eq!(body["content"][0]["text"], "mock responses reply");
    assert_eq!(body["usage"]["output_tokens"], 5);

    let calls = calls_to(&mock, "responses").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], "mock-resp");
    assert!(calls[0]["input"].is_array());
}

#[tokio::test]
async fn e2e_copilot_responses_endpoint_routes_to_responses() {
    // github-copilot aisdk models declare `settings.endpoint: "responses"`
    // (GPT-6/5.6, grok, mai-code, codex). Without the endpoint-aware routing
    // they were sent to /chat/completions -> 400 "not accessible via the
    // /chat/completions endpoint".
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let e = CatalogEntry {
        id: "grok-4.7".to_string(),
        model_id: "grok-4.7".to_string(),
        provider_id: "opencode".to_string(),
        name: "Grok 4.7".to_string(),
        package: "aisdk:@ai-sdk/github-copilot".to_string(),
        settings: CatalogSettings {
            base_url: Some(base.clone()),
            api_key: Some("test-upstream-key".to_string()),
            provider: None,
            endpoint: Some("responses".to_string()),
        },
        limit: None,
        enabled: true,
        variants: vec![],
        headers: None,
        body: None,
    };
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-github-copilot-grok-4-7", "opencode/grok-4.7")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-github-copilot-grok-4-7"))
        .await;
    assert_eq!(resp.status_code(), 200);
    // The upstream must have seen a Responses-API call, not chat/completions.
    let calls = calls_to(&mock, "responses").await;
    assert_eq!(calls.len(), 1, "expected the request on /responses");
    assert_eq!(calls[0]["model"], "grok-4.7");
    assert!(calls_to(&mock, "chat/completions").await.is_empty());
}

#[tokio::test]
async fn e2e_copilot_chat_endpoint_stays_on_chat_completions() {
    // Gemini copilot models declare `settings.endpoint: "chat"` and must
    // keep routing to /chat/completions (regression guard).
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let e = CatalogEntry {
        id: "gemini-3.6-flash".to_string(),
        model_id: "gemini-3.6-flash".to_string(),
        provider_id: "opencode".to_string(),
        name: "Gemini 3.6 Flash".to_string(),
        package: "aisdk:@ai-sdk/github-copilot".to_string(),
        settings: CatalogSettings {
            base_url: Some(base.clone()),
            api_key: Some("test-upstream-key".to_string()),
            provider: None,
            endpoint: Some("chat".to_string()),
        },
        limit: None,
        enabled: true,
        variants: vec![],
        headers: None,
        body: None,
    };
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias(
            "claude-github-copilot-gemini-3-6-flash",
            "opencode/gemini-3.6-flash",
        )],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-github-copilot-gemini-3-6-flash"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "chat/completions").await;
    assert_eq!(calls.len(), 1, "expected the request on /chat/completions");
    assert_eq!(calls[0]["model"], "gemini-3.6-flash");
    assert!(calls_to(&mock, "responses").await.is_empty());
}

#[tokio::test]
async fn e2e_anthropic_passthrough() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, ANTHROPIC_PKG, "mock-anth")],
        vec![alias("claude-mock-anth", "opencode/mock-anth")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-anth"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    // Gateway rewrites the upstream model to the gateway id for the client.
    assert_eq!(body["model"], "claude-mock-anth");
    assert_eq!(body["content"][0]["text"], "mock anthropic reply");

    let calls = calls_to(&mock, "messages").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], "mock-anth");
    assert_eq!(calls[0]["max_tokens"], 8);
}

#[tokio::test]
async fn e2e_fast_flavor_keeps_distinct_alias_and_headers() {
    use std::collections::HashMap;
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    // Provider `opencode` with inline api_key needs no credential DB (same
    // trick as `mock_entry`); the disambiguation logic is provider-agnostic.
    let mut normal = mock_entry(&base, ANTHROPIC_PKG, "claude-opus-4.8");
    normal.id = "claude-opus-4.8".to_string();
    normal.name = "Claude Opus 4.8".to_string();
    let mut fast = mock_entry(&base, ANTHROPIC_PKG, "claude-opus-4.8");
    fast.id = "claude-opus-4.8-fast".to_string();
    fast.name = "Claude Opus 4.8 Fast".to_string();
    fast.headers = Some(HashMap::from([(
        "anthropic-beta".to_string(),
        "fast-mode-2026-02-01".to_string(),
    )]));
    fast.body = Some(json!({"speed": "fast"}));
    // Aliases built the same way `refresh()` does: no duplicate ids.
    // Shield off here: this test covers fast-flavor disambiguation, not the
    // CLI family shield (covered by the domain shield tests).
    let aliases =
        frank_opencode::domain::auto_aliases_for(&[normal.clone(), fast.clone()], false, false);
    assert_eq!(aliases.len(), 2);
    assert_ne!(aliases[0].gateway_id, aliases[1].gateway_id);
    let state = seeded_state(test_config(), vec![normal, fast], aliases.clone()).await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    for a in &aliases {
        let resp = server
            .post("/v1/messages")
            .add_header("x-api-key", "test-secret")
            .json(&msg_body(&a.gateway_id))
            .await;
        assert_eq!(resp.status_code(), 200, "alias {}", a.gateway_id);
    }
    let calls = calls_to(&mock, "messages").await;
    assert_eq!(calls.len(), 2);
    // Both hit the same upstream model id, but the fast row carries speed.
    assert!(calls.iter().all(|c| c["model"] == "claude-opus-4.8"));
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.get("speed") == Some(&json!("fast")))
            .count(),
        1
    );
    // Fast beta header reached the upstream exactly once.
    let betas = mock.betas.lock().await;
    assert_eq!(
        betas
            .iter()
            .filter(|b| b.as_deref() == Some("fast-mode-2026-02-01"))
            .count(),
        1,
        "{betas:?}"
    );
}

#[tokio::test]
async fn e2e_provider_id_ref_resolves_fast_row() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, ANTHROPIC_PKG, "claude-opus-4.8");
    e.id = "claude-opus-4.8-fast".to_string();
    e.body = Some(json!({"speed": "fast"}));
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-fast", "opencode/claude-opus-4.8-fast")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    // Direct provider/id and plain id both resolve via the `id` match.
    for model in ["opencode/claude-opus-4.8-fast", "claude-opus-4.8-fast"] {
        let resp = server
            .post("/v1/messages")
            .add_header("x-api-key", "test-secret")
            .json(&msg_body(model))
            .await;
        assert_eq!(resp.status_code(), 200, "model {model}");
    }
    let calls = calls_to(&mock, "messages").await;
    assert_eq!(calls.len(), 2);
}

#[tokio::test]
async fn e2e_catalog_body_merged_into_upstream() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, ANTHROPIC_PKG, "mock-anth");
    e.body = Some(json!({"speed": "fast"}));
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-anth", "opencode/mock-anth")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-anth"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "messages").await;
    assert_eq!(calls[0]["speed"], "fast");
}

#[tokio::test]
async fn e2e_tool_choice_none_reaches_upstream() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, CHAT_PKG, "mock-chat")],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body_tools("claude-mock-chat"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "chat/completions").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["tool_choice"], "none");
    assert_eq!(calls[0]["tools"][0]["function"]["name"], "Read");
}

#[tokio::test]
async fn e2e_chat_streaming_translated_to_anthropic_sse() {
    let mock = MockUpstream {
        chat_streaming: true,
        ..MockUpstream::default()
    };
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, CHAT_PKG, "mock-chat")],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let mut body = msg_body("claude-mock-chat");
    body["stream"] = Value::Bool(true);
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&body)
        .await;
    assert_eq!(resp.status_code(), 200);
    assert_eq!(
        resp.header("content-type").to_str().unwrap(),
        "text/event-stream"
    );
    let text = resp.text();
    assert!(text.contains("message_start"), "{text}");
    assert!(text.contains("text_delta"), "{text}");
    assert!(text.contains("hel"), "{text}");
    assert!(text.contains("lo"), "{text}");
    assert!(text.contains("message_stop"), "{text}");
    // Upstream got the streaming request.
    let calls = calls_to(&mock, "chat/completions").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["stream"], true);
}

#[tokio::test]
async fn e2e_variant_adds_reasoning_effort() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    // Variant support is fed from the catalog (`variants` field), same as
    // `opencode api get /api/model` reports it.
    let mut e = mock_entry(&base, CHAT_PKG, "mock-chat");
    e.variants.push(frank_opencode::domain::ModelVariant {
        id: "high".to_string(),
        settings: frank_opencode::domain::ModelVariantSettings {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        },
    });
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-chat#high"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "chat/completions").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["model"], "mock-chat");
    assert_eq!(calls[0]["reasoning_effort"], "high");
}

#[tokio::test]
async fn unknown_variant_is_404_with_available_list() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, CHAT_PKG, "mock-chat");
    e.variants.push(frank_opencode::domain::ModelVariant {
        id: "high".to_string(),
        settings: frank_opencode::domain::ModelVariantSettings {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        },
    });
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-chat#turbo"))
        .await;
    assert_eq!(resp.status_code(), 404);
    let body: Value = resp.json();
    let msg = body["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("high"), "{msg}");
}

#[tokio::test]
async fn count_tokens_proxies_anthropic_upstream_and_falls_back() {
    // The mock upstream counts tokens for the Anthropic package.
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let anth = mock_entry(&base, ANTHROPIC_PKG, "mock-anth");
    let chat = mock_entry(&base, CHAT_PKG, "mock-chat");
    let state = seeded_state(
        test_config(),
        vec![anth, chat],
        vec![
            alias("claude-mock-anth", "opencode/mock-anth"),
            alias("claude-mock-chat", "opencode/mock-chat"),
        ],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    // 1) Without a count handler route, the proxy fails -> estimate fallback.
    let resp = server
        .post("/v1/messages/count_tokens")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-anth"))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.json::<Value>().get("input_tokens").is_some());
    // 2) Non-Anthropic package: local estimate, no upstream call.
    let resp = server
        .post("/v1/messages/count_tokens")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-chat"))
        .await;
    assert_eq!(resp.status_code(), 200);
    assert!(resp.json::<Value>().get("input_tokens").is_some());
}

#[tokio::test]
async fn count_tokens_missing_messages_is_400() {
    let base = spawn_mock(MockUpstream::default()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, CHAT_PKG, "mock-chat")],
        vec![alias("claude-x", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages/count_tokens")
        .add_header("x-api-key", "test-secret")
        .json(&json!({"model": "claude-x", "system": "s"}))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: Value = resp.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn e2e_anthropic_variant_sets_thinking() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, ANTHROPIC_PKG, "mock-anth");
    e.variants.push(frank_opencode::domain::ModelVariant {
        id: "high".to_string(),
        settings: frank_opencode::domain::ModelVariantSettings {
            thinking: Some(frank_opencode::domain::ThinkingConfig::Typed {
                kind: "adaptive".to_string(),
                display: Some("summarized".to_string()),
            }),
            ..Default::default()
        },
    });
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-anth", "opencode/mock-anth")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-anth#high"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "messages").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["thinking"]["type"], "adaptive");
    assert_eq!(calls[0]["thinking"]["display"], "summarized");
}

#[tokio::test]
async fn e2e_unrepresentable_anthropic_variant_is_400() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, ANTHROPIC_PKG, "mock-anth");
    // `reasoningEffort` has no Messages API parameter: must fail loudly.
    e.variants.push(frank_opencode::domain::ModelVariant {
        id: "high".to_string(),
        settings: frank_opencode::domain::ModelVariantSettings {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        },
    });
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-anth", "opencode/mock-anth")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body("claude-mock-anth#high"))
        .await;
    assert_eq!(resp.status_code(), 400);
    let body: Value = resp.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    // The request never reached the upstream.
    assert!(calls_to(&mock, "messages").await.is_empty());
}

#[tokio::test]
async fn e2e_count_tokens_applies_variant_and_catalog_body() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let mut e = mock_entry(&base, ANTHROPIC_PKG, "mock-anth");
    e.body = Some(json!({"speed": "fast"}));
    e.variants.push(frank_opencode::domain::ModelVariant {
        id: "high".to_string(),
        settings: frank_opencode::domain::ModelVariantSettings {
            thinking: Some(frank_opencode::domain::ThinkingConfig::Typed {
                kind: "adaptive".to_string(),
                display: None,
            }),
            ..Default::default()
        },
    });
    let state = seeded_state(
        test_config(),
        vec![e],
        vec![alias("claude-mock-anth", "opencode/mock-anth")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages/count_tokens")
        .add_header("x-api-key", "test-secret")
        .json(&json!({
            "model": "claude-mock-anth#high",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;
    assert_eq!(resp.status_code(), 200);
    let calls = calls_to(&mock, "messages/count_tokens").await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["thinking"]["type"], "adaptive");
    assert_eq!(calls[0]["speed"], "fast");
    // Streaming is a request-only concern: never forwarded to count_tokens.
    assert!(calls[0].get("stream").is_none());
}

#[tokio::test]
async fn e2e_chat_stream_failure_surfaces_error_event() {
    let mock = MockUpstream {
        chat_stream_truncated: true,
        ..MockUpstream::default()
    };
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(),
        vec![mock_entry(&base, CHAT_PKG, "mock-chat")],
        vec![alias("claude-mock-chat", "opencode/mock-chat")],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let mut body = msg_body("claude-mock-chat");
    body["stream"] = Value::Bool(true);
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&body)
        .await;
    assert_eq!(resp.status_code(), 200);
    let text = resp.text();
    // Partial content still delivered, then an explicit error frame.
    assert!(text.contains("partial"), "{text}");
    assert!(text.contains("message_stop"), "{text}");
    assert!(text.contains("event: error"), "{text}");
}

// ---------------------------------------------------------------------------
// Boot catalog retry (issue #1): a fake `opencode` shim whose behavior is
// driven by a state file, rewritten by the test between retries. `refresh()`
// runs the `opencode_bin` command, so we point AppConfig at an absolute path
// to a temp shim script — PATH left untouched, tests stay parallel-safe.
// ---------------------------------------------------------------------------

const RETRY_ATTEMPTS: usize = 6;
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

fn shim_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "frank-retry-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

/// State file the shim reads on every call. Phase 1 = empty catalog,
/// phase 2 = catalog with one model, `fail` = exit non-zero.
enum ShimPhase {
    Empty,
    Fail,
    Model,
}

fn shim_state_file() -> PathBuf {
    std::env::temp_dir().join(format!(
        "frank-retry-state-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

fn write_phase(state_file: &PathBuf, phase: &ShimPhase) {
    std::fs::write(
        state_file,
        match phase {
            ShimPhase::Empty => "empty",
            ShimPhase::Fail => "fail",
            ShimPhase::Model => "model",
        },
    )
    .expect("write shim state");
}

/// JSON body on stdout for one enabled model with the given gateway id.
fn catalog_body(model: &str) -> Value {
    json!({
        "data": [{
            "id": model,
            "modelID": model,
            "providerID": "opencode",
            "name": model,
            "package": "@opencode/ai/providers/anthropic",
            "settings": {},
            "limit": {"context": 1_000_000},
            "enabled": true,
            "variants": []
        }]
    })
}

/// Write a self-contained `opencode` shim: it reads its phase from the state
/// file and prints a preset catalog (or exits 1 for `fail`). The `model`
/// phase prints the catalog for `model`.
fn write_shim(dir: &std::path::Path, state_file: &std::path::Path, model: &str) -> String {
    let body = catalog_body(model).to_string();
    std::fs::create_dir_all(dir).expect("create shim dir");
    let path = dir.join("opencode");
    let script = format!(
        "#!/bin/sh\n\
         phase=\"$(cat '{}')\"\n\
         if [ \"$phase\" = \"fail\" ]; then\n  exit 1\nfi\n\
         if [ \"$phase\" = \"model\" ]; then\n  cat <<'EOM'\n{body}\nEOM\n  exit 0\nfi\n\
         cat <<'EOM'\n{{\"data\":[]}}\nEOM\n",
        state_file.display()
    );
    std::fs::write(&path, script).expect("write shim");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod shim");
    path.to_string_lossy().to_string()
}

/// State whose AppConfig points `opencode_bin` at a shim script.
/// `include_free_tier` is set so the shim's `opencode/*` model counts as
/// usable (otherwise `refresh()` filters free-tier entries out and the
/// catalog is always empty no matter what the shim returns).
async fn shim_state(bin: &str) -> AppState {
    let mut cfg = test_config();
    cfg.opencode_bin = bin.to_string();
    cfg.include_free_tier = true;
    seeded_state(cfg, vec![], vec![]).await
}

#[tokio::test]
async fn boot_retry_converges_when_catalog_starts_empty() {
    let dir = shim_dir();
    let state_file = shim_state_file();
    write_phase(&state_file, &ShimPhase::Empty);
    let state = shim_state(&write_shim(&dir, &state_file, "boot-retry-model")).await;

    // The catalog warms up after the first attempts: flip the phase while the
    // retry is sleeping between attempts. Early attempts see `Empty` and are
    // retried; a later attempt picks up the populated catalog.
    let state_file2 = state_file.clone();
    let flipper = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        write_phase(&state_file2, &ShimPhase::Model);
    });
    let n = state
        .refresh_with_retry(RETRY_ATTEMPTS, RETRY_BACKOFF)
        .await;
    flipper.await.expect("flipper join");

    assert_eq!(n, 1);
    // The converged load cleared the transient empty error.
    assert!(state.last_error.read().await.is_none());
    let aliases = state.aliases.read().await;
    assert!(aliases
        .iter()
        .any(|a| a.gateway_id == "claude-opencode-boot-retry-model"));
}

#[tokio::test]
async fn boot_retry_marks_degraded_on_persistent_empty() {
    let dir = shim_dir();
    let state_file = shim_state_file();
    write_phase(&state_file, &ShimPhase::Empty);
    let state = shim_state(&write_shim(&dir, &state_file, "never-model")).await;

    let n = state
        .refresh_with_retry(RETRY_ATTEMPTS, RETRY_BACKOFF)
        .await;
    assert_eq!(n, 0);
    let err = state
        .last_error
        .read()
        .await
        .clone()
        .expect("last_error set");
    assert!(
        err.contains("still empty after 6 attempts"),
        "unexpected: {err}"
    );
}

#[tokio::test]
async fn boot_retry_recovers_after_failures() {
    let dir = shim_dir();
    let state_file = shim_state_file();
    write_phase(&state_file, &ShimPhase::Fail);
    let state = shim_state(&write_shim(&dir, &state_file, "recovered-model")).await;

    // Fetch failures (exit 1) initially, then the service becomes healthy.
    let state_file2 = state_file.clone();
    let flipper = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        write_phase(&state_file2, &ShimPhase::Model);
    });
    let n = state
        .refresh_with_retry(RETRY_ATTEMPTS, RETRY_BACKOFF)
        .await;
    flipper.await.expect("flipper join");

    assert_eq!(n, 1);
    assert!(state.last_error.read().await.is_none());
    let aliases = state.aliases.read().await;
    assert!(aliases
        .iter()
        .any(|a| a.gateway_id == "claude-opencode-recovered-model"));
}

// ---------------------------------------------------------------------------
// cli_shield_aliases: refresh() shields family spelling on auto aliases by
// default; manual aliases and direct refs still resolve.
// ---------------------------------------------------------------------------

/// Shim catalog body with two rows sharing a modelID: a Copilot Claude row
/// (family spelling) and a neutral row.
fn shield_catalog_body() -> Value {
    json!({
        "data": [
            {
                "id": "claude-sonnet-5",
                "modelID": "claude-sonnet-5",
                "providerID": "github-copilot",
                "name": "Claude Sonnet 5",
                "package": "aisdk:@ai-sdk/github-copilot",
                "settings": {"baseURL": "http://127.0.0.1:9", "endpoint": "messages"},
                "enabled": true,
                "variants": []
            },
            {
                "id": "kimi-k2.7-code",
                "modelID": "kimi-k2.7-code",
                "providerID": "opencode-go",
                "name": "Kimi K2.7 Code",
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": {"baseURL": "http://127.0.0.1:9"},
                "enabled": true,
                "variants": []
            }
        ]
    })
}

fn write_shield_shim(dir: &std::path::Path) -> String {
    let body = shield_catalog_body().to_string();
    std::fs::create_dir_all(dir).expect("create shim dir");
    let path = dir.join("opencode");
    let script = format!("#!/bin/sh\ncat <<'EOM'\n{body}\nEOM\n");
    std::fs::write(&path, script).expect("write shim");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod shim");
    path.to_string_lossy().to_string()
}

#[tokio::test]
async fn refresh_shields_family_spelling_by_default() {
    let dir = shim_dir();
    let mut cfg = test_config();
    cfg.opencode_bin = write_shield_shim(&dir);
    // Default: shield on (AppConfig::default has cli_shield_aliases = true).
    assert!(cfg.cli_shield_aliases);
    let state = seeded_state(cfg, vec![], vec![]).await;
    let n = state.refresh().await.expect("refresh");
    assert_eq!(n, 2);
    let aliases = state.aliases.read().await;
    let ids: Vec<&str> = aliases.iter().map(|a| a.gateway_id.as_str()).collect();
    // Family spelling gone from the advertised Copilot id; neutral row kept.
    assert!(ids.contains(&"claude-github-copilot-cs-5"), "{ids:?}");
    assert!(
        !ids.iter().any(|id| id.contains("claude-sonnet")),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"claude-opencode-go-kimi-k2-7-code"),
        "{ids:?}"
    );
    // The shielded alias resolves to the Copilot row for /v1/messages.
    let data: Value = axum_test::TestServer::new(router(state.clone()))
        .unwrap()
        .get("/v1/models")
        .add_header("x-api-key", "test-secret")
        .await
        .json();
    assert!(data["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "claude-github-copilot-cs-5"));
}

#[tokio::test]
async fn refresh_keeps_historical_spelling_when_shield_off() {
    let dir = shim_dir();
    let mut cfg = test_config();
    cfg.opencode_bin = write_shield_shim(&dir);
    cfg.cli_shield_aliases = false;
    let state = seeded_state(cfg, vec![], vec![]).await;
    state.refresh().await.expect("refresh");
    let aliases = state.aliases.read().await;
    let ids: Vec<&str> = aliases.iter().map(|a| a.gateway_id.as_str()).collect();
    assert!(
        ids.contains(&"claude-github-copilot-claude-sonnet-5"),
        "{ids:?}"
    );
}

// ---------------------------------------------------------------------------
// mock_classifier: auto-mode safety checks answered locally, upstream
// untouched; real conversations still forwarded.
// ---------------------------------------------------------------------------

fn classifier_config() -> AppConfig {
    AppConfig {
        mock_classifier: true,
        ..test_config()
    }
}

/// Stage-1-shaped auto-mode classifier body (observed on the wire:
/// max_tokens 64, tools [], system as content blocks, no stream).
fn classifier_body(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "system": [{"type": "text", "text": "Respond with <severity>N</severity> ONLY."}],
        "messages": [
            {"role": "user", "content": "action context"},
            {"role": "user", "content": "more context"},
        ],
        "tools": [],
        "tool_choice": null,
    })
}

#[tokio::test]
async fn e2e_mock_classifier_answered_locally_without_upstream() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        classifier_config(),
        vec![mock_entry(&base, ANTHROPIC_PKG, "claude-sonnet-5")],
        vec![],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&classifier_body("claude-sonnet-5"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "claude-sonnet-5");
    assert_eq!(body["content"][0]["text"], "<severity>0</severity>");
    assert!(
        calls_to(&mock, "messages").await.is_empty(),
        "classifier call must never reach upstream"
    );
}

#[tokio::test]
async fn e2e_mock_probe_answered_locally() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    // The probe carries the alias id; interception runs before resolve, so
    // it answers even though no entry/alias exists for it.
    let state = seeded_state(
        classifier_config(),
        vec![mock_entry(&base, ANTHROPIC_PKG, "claude-sonnet-5")],
        vec![],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let probe = json!({
        "model": "claude-github-copilot-claude-sonnet-5",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "x"}],
    });
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&probe)
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["content"][0]["text"], "ok");
    assert_eq!(body["model"], "claude-github-copilot-claude-sonnet-5");
    assert!(calls_to(&mock, "messages").await.is_empty());
}

#[tokio::test]
async fn e2e_mock_disabled_still_forwards_classifier_body() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        test_config(), // mock_classifier defaults to false
        vec![mock_entry(&base, ANTHROPIC_PKG, "claude-sonnet-5")],
        vec![],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&classifier_body("claude-sonnet-5"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["content"][0]["text"], "mock anthropic reply");
    assert_eq!(calls_to(&mock, "messages").await.len(), 1);
}

#[tokio::test]
async fn e2e_mock_real_conversation_still_forwards() {
    let mock = MockUpstream::default();
    let base = spawn_mock(mock.clone()).await;
    let state = seeded_state(
        classifier_config(),
        vec![mock_entry(&base, ANTHROPIC_PKG, "claude-sonnet-5")],
        vec![],
    )
    .await;
    let server = axum_test::TestServer::new(router(state)).unwrap();
    // Real agent turn: tools present → never mocked, even with mock on.
    let resp = server
        .post("/v1/messages")
        .add_header("x-api-key", "test-secret")
        .json(&msg_body_tools("claude-sonnet-5"))
        .await;
    assert_eq!(resp.status_code(), 200);
    let body: Value = resp.json();
    assert_eq!(body["content"][0]["text"], "mock anthropic reply");
    assert_eq!(calls_to(&mock, "messages").await.len(), 1);
}
