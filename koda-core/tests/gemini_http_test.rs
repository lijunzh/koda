//! HTTP-layer integration tests for [`GeminiProvider`] using
//! [`koda_test_utils::network::FakeLlmServer`].
//!
//! Companion to `openai_compat_http_test.rs` and `anthropic_http_test.rs`.
//! Gemini differs from both:
//!
//! * Non-streaming endpoint: `POST /v1beta/models/{model}:generateContent`.
//! * Streaming endpoint:     `POST /v1beta/models/{model}:streamGenerateContent?alt=sse`.
//! * Auth lives in the URL **query string** (`?key=…`), not in a header.
//! * SSE chunks have a different shape: `candidates[].content.parts[].text`.
//!
//! These tests pin all four behaviors against a real reqwest round-trip.
//!
//! ## Path-regex notes
//!
//! Gemini's two endpoints differ only by the `stream` prefix. Anchor the
//! non-streaming matcher with `:generateContent$` so it does **not** also
//! match `:streamGenerateContent` (the substring before `generateContent`
//! is `:` for non-stream vs `m` for stream — the leading `:` in the regex
//! disambiguates).

use koda_core::config::{ModelSettings, ProviderType};
use koda_core::providers::gemini::GeminiProvider;
use koda_core::providers::{ChatMessage, LlmProvider, StreamChunk};
use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};

/// Minimal well-formed `:generateContent` non-streaming response.
fn ok_generate_body() -> Value {
    json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{ "text": "ok" }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 1,
            "candidatesTokenCount": 1,
            "cachedContentTokenCount": 0,
            "thoughtsTokenCount": 0
        }
    })
}

fn settings() -> ModelSettings {
    ModelSettings::defaults_for("gemini-2.5-flash", &ProviderType::Gemini)
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage::text("user", text)
}

// ── chat() ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_sends_post_to_generate_content_endpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*:generateContent$", ok_generate_body())
        .await;

    let provider = GeminiProvider::new("gem-test".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat must succeed against 200 mock");

    let reqs = server.received_requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert!(
        reqs[0].url.path().ends_with(":generateContent"),
        "wrong path: {}",
        reqs[0].url.path()
    );
    assert!(
        reqs[0]
            .url
            .path()
            .contains("/v1beta/models/gemini-2.5-flash:"),
        "model must be embedded in the path, got: {}",
        reqs[0].url.path()
    );
}

#[tokio::test]
async fn chat_sends_api_key_as_query_param_not_header() {
    // Gemini's defining quirk: API key goes in `?key=…`, NOT in any header.
    // This is the test that matters most for an HTTP-layer suite.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*:generateContent$", ok_generate_body())
        .await;

    let provider = GeminiProvider::new("gem-secret-123".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;

    // Auth via query param.
    let query = reqs[0].url.query().unwrap_or("");
    assert!(
        query.contains("key=gem-secret-123"),
        "API key must appear in query string, got: {query}"
    );

    // No auth headers (Gemini sends none of these).
    assert!(reqs[0].headers.get("authorization").is_none());
    assert!(reqs[0].headers.get("x-api-key").is_none());
    assert!(reqs[0].headers.get("x-goog-api-key").is_none());
}

#[tokio::test]
async fn chat_returns_error_on_5xx_with_status_in_message() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*:generateContent$",
            503,
            r#"{"error":{"code":503,"message":"service unavailable","status":"UNAVAILABLE"}}"#,
        )
        .await;

    let provider = GeminiProvider::new("k".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("5xx must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("503"), "error must mention status: {msg}");
    assert!(msg.contains("UNAVAILABLE"), "error must include body");
}

#[tokio::test]
async fn chat_returns_error_on_400_invalid_request() {
    // Regression guard: pins current 'no-retry-on-4xx' behavior.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*:generateContent$",
            400,
            r#"{"error":{"code":400,"message":"bad request"}}"#,
        )
        .await;

    let provider = GeminiProvider::new("k".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("400 must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("400"), "error must mention status: {msg}");
}

// ── chat_stream() ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_consumes_gemini_sse_format_via_real_tcp() {
    // Gemini SSE chunks: each line is a full `GenerateResponse` JSON with
    // a `candidates[].content.parts[].text` field. Final chunk carries the
    // `finishReason` and `usageMetadata` totals.
    //
    // Note: Gemini does NOT send a `[DONE]` sentinel — the stream just
    // closes after the last chunk.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*:streamGenerateContent$",
            &[
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hel"}]}}]}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"lo "}]}}]}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"cachedContentTokenCount":0,"thoughtsTokenCount":0}}"#,
            ],
        )
        .await;

    let provider = GeminiProvider::new("k".into(), Some(&server.url()));
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

#[tokio::test]
async fn chat_stream_sends_alt_sse_query_param() {
    // Regression guard: the streaming endpoint must include `alt=sse` so
    // Gemini returns SSE-framed chunks instead of newline-delimited JSON.
    // Drop this and the stream silently switches format.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*:streamGenerateContent$",
            &[
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"x"}]},"finishReason":"STOP"}]}"#,
            ],
        )
        .await;

    let provider = GeminiProvider::new("k".into(), Some(&server.url()));
    let mut collector = provider
        .chat_stream(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();
    // Drain so the request completes.
    while collector.rx.recv().await.is_some() {}

    let reqs = server.received_requests().await;
    let query = reqs[0].url.query().unwrap_or("");
    assert!(
        query.contains("alt=sse"),
        "streaming request must include alt=sse, got: {query}"
    );
    assert!(
        query.contains("key=k"),
        "streaming request must include API key, got: {query}"
    );
}
