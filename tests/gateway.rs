//! Integration tests against a seeded gateway (no `opencode` binary needed).

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
        },
        limit: None,
        enabled: true,
        variants: vec![],
    }
}

fn alias(gateway: &str, opencode_ref: &str) -> AliasEntry {
    AliasEntry {
        gateway_id: gateway.to_string(),
        opencode_ref: opencode_ref.to_string(),
        display_name: gateway.to_string(),
        description: "test".to_string(),
        context_window: None,
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
    assert_eq!(item["id"], "claude-x");
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
    chat_streaming: bool,
}

async fn note(st: &MockUpstream, headers: &axum::http::HeaderMap, path: &str, body: Value) {
    st.calls.lock().await.push((path.to_string(), body));
    if let Some(s) = headers
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
    {
        st.sessions.lock().await.push(s.to_string());
    }
}

async fn mock_chat(
    State(st): State<MockUpstream>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    note(&st, &headers, "chat/completions", body).await;
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

async fn spawn_mock(mock: MockUpstream) -> String {
    let app = Router::new()
        .route("/chat/completions", post(mock_chat))
        .route("/responses", post(mock_responses))
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
        },
        limit: None,
        enabled: true,
        variants: vec![],
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
            thinking: None,
            budget_tokens: None,
            include: None,
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
            thinking: None,
            budget_tokens: None,
            include: None,
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
