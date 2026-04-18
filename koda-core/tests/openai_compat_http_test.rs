//! HTTP-layer integration tests for [`OpenAiCompatProvider`] using
//! [`koda_test_utils::network::FakeLlmServer`].
//!
//! These tests complement the in-module unit tests at the bottom of
//! `koda-core/src/providers/openai_compat.rs`, which cover request
//! construction and response deserialization against static strings.
//! The tests here exercise the *actual* `reqwest::Client` round-trip:
//! URL routing, bearer-auth header injection, status-code error paths,
//! and SSE chunk framing over a real TCP socket on loopback.
//!
//! Together they form the "HTTP integration coverage" leg of #858 Phase 2
//! (kicked off in this PR for `openai_compat`; follow-ups extend the same
//! pattern to `anthropic` and `gemini`).

use koda_core::config::{ModelSettings, ProviderType};
use koda_core::providers::openai_compat::OpenAiCompatProvider;
use koda_core::providers::{ChatMessage, LlmProvider, StreamChunk};
use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};

/// Minimal, well-formed `chat/completions` response body.
fn ok_chat_body() -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

fn settings() -> ModelSettings {
    ModelSettings::defaults_for("gpt-4o", &ProviderType::OpenAI)
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage::text("user", text)
}

// ── chat() ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_sends_post_to_chat_completions_endpoint() {
    let server = FakeLlmServer::spawn().await;
    server.mount_chat_ok(ok_chat_body()).await;

    let provider = OpenAiCompatProvider::new(&server.url(), Some("sk-test".into()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat must succeed against 200 mock");

    let reqs = server.received_requests().await;
    assert_eq!(reqs.len(), 1, "exactly one POST expected");
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert!(
        reqs[0].url.path().ends_with("/chat/completions"),
        "wrong path: {}",
        reqs[0].url.path()
    );
}

#[tokio::test]
async fn chat_includes_bearer_token_when_api_key_set() {
    let server = FakeLlmServer::spawn().await;
    server.mount_chat_ok(ok_chat_body()).await;

    let provider = OpenAiCompatProvider::new(&server.url(), Some("sk-secret-123".into()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let auth = reqs[0]
        .headers
        .get("authorization")
        .expect("Authorization header required when api_key is set");
    assert_eq!(auth, "Bearer sk-secret-123");
}

#[tokio::test]
async fn chat_omits_authorization_header_when_no_api_key() {
    let server = FakeLlmServer::spawn().await;
    server.mount_chat_ok(ok_chat_body()).await;

    let provider = OpenAiCompatProvider::new(&server.url(), None);
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "Authorization header must NOT be present when api_key is None"
    );
}

#[tokio::test]
async fn chat_returns_error_on_5xx_with_status_in_message() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_chat_status(503, r#"{"error":"upstream down"}"#)
        .await;

    let provider = OpenAiCompatProvider::new(&server.url(), Some("k".into()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("5xx must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("503"), "error must mention status: {msg}");
    assert!(
        msg.contains("upstream down"),
        "error must include body: {msg}"
    );
}

#[tokio::test]
async fn chat_returns_error_on_4xx_unauthorized() {
    // Regression guard: the same error path must fire for 4xx as for 5xx
    // (the provider doesn't special-case auth errors yet — this test pins
    // current behavior so any future retry-on-4xx work is intentional).
    let server = FakeLlmServer::spawn().await;
    server
        .mount_chat_status(401, r#"{"error":"invalid_api_key"}"#)
        .await;

    let provider = OpenAiCompatProvider::new(&server.url(), Some("bad".into()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("401 must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error must mention status: {msg}");
}

// ── chat_stream() ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_consumes_sse_chunks_via_real_tcp() {
    // OpenAI streaming format: each SSE `data:` line is a JSON delta.
    // We send three deltas plus the standard `[DONE]` sentinel and assert
    // the SseCollector reassembles the full text.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_chat_sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"hel"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"lo "},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"world"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
        .await;

    let provider = OpenAiCompatProvider::new(&server.url(), Some("k".into()));
    let mut collector = provider
        .chat_stream(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat_stream must succeed");

    let mut text = String::new();
    while let Some(chunk) = collector.rx.recv().await {
        if let StreamChunk::TextDelta(s) = chunk {
            text.push_str(&s);
        }
    }

    assert_eq!(text, "hello world", "all SSE deltas must be reassembled");
}
