//! Integration tests against a seeded gateway (no `opencode` binary needed).

use frank_opencode::api::server::{router, AppState};
use frank_opencode::config::AppConfig;
use frank_opencode::domain::{AliasEntry, CatalogEntry, CatalogSettings};
use serde_json::{json, Value};
use std::path::PathBuf;

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
        enabled: true,
    }
}

fn alias(gateway: &str, opencode_ref: &str) -> AliasEntry {
    AliasEntry {
        gateway_id: gateway.to_string(),
        opencode_ref: opencode_ref.to_string(),
        display_name: gateway.to_string(),
        description: "test".to_string(),
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
