//! HTTP-layer integration tests for [`AnthropicProvider`] using
//! [`koda_test_utils::network::FakeLlmServer`].
//!
//! Companion to `openai_compat_http_test.rs`. Anthropic differs from
//! OpenAI in three ways that motivate dedicated tests:
//!
//! * Endpoint is `POST /v1/messages` (not `/chat/completions`).
//! * Auth is the `x-api-key` header (not `Authorization: Bearer …`).
//! * `anthropic-version` is required on every request.
//!
//! These tests pin all three behaviors against a real reqwest round-trip.

use koda_core::config::{ModelSettings, ProviderType};
use koda_core::providers::anthropic::AnthropicProvider;
use koda_core::providers::{ChatMessage, LlmProvider, StreamChunk};
use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};

/// Minimal well-formed `/v1/messages` non-streaming response.
fn ok_messages_body() -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{ "type": "text", "text": "ok" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
}

fn settings() -> ModelSettings {
    ModelSettings::defaults_for("claude-sonnet-4-5-20250929", &ProviderType::Anthropic)
}

fn user_msg(text: &str) -> ChatMessage {
    ChatMessage::text("user", text)
}

// ── chat() ────────────────────────────────────────────────────────────────

/// `input_tokens` leaves out cached tokens, but they still fill the window.
#[test]
fn context_tokens_counts_the_cache() {
    let provider = AnthropicProvider::new("sk-ant-test".into(), None);
    let usage = koda_core::providers::TokenUsage {
        prompt_tokens: 288,
        cache_read_tokens: 20_000,
        cache_creation_tokens: 3_535,
        ..Default::default()
    };
    assert_eq!(provider.context_tokens(&usage), 23_823);
}

/// Compaction reads this to tell a reply cut off at the cap from a finished one.
#[tokio::test]
async fn chat_reports_the_stop_reason() {
    let server = FakeLlmServer::spawn().await;
    let mut body = ok_messages_body();
    body["content"] = serde_json::json!([]);
    body["stop_reason"] = serde_json::json!("max_tokens");
    server.mount_ok("POST", r".*/v1/messages$", body).await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    let reply = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    assert_eq!(reply.usage.stop_reason, "max_tokens");
}

#[tokio::test]
async fn chat_skips_thinking_blocks_in_the_reply() {
    let server = FakeLlmServer::spawn().await;
    let mut body = ok_messages_body();
    body["content"] = serde_json::json!([
        {"type": "thinking", "thinking": "Let me plan.", "signature": "sig"},
        {"type": "redacted_thinking", "data": "abc"},
        {"type": "text", "text": "The summary."}
    ]);
    server.mount_ok("POST", r".*/v1/messages$", body).await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    let reply = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("a thinking block must not fail the response");

    assert_eq!(reply.content.as_deref(), Some("The summary."));
}

#[tokio::test]
async fn chat_sends_post_to_v1_messages_endpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect("chat must succeed against 200 mock");

    let reqs = server.received_requests().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert!(
        reqs[0].url.path().ends_with("/v1/messages"),
        "wrong path: {}",
        reqs[0].url.path()
    );
}

#[tokio::test]
async fn chat_sends_x_api_key_header_not_bearer() {
    // Anthropic uses `x-api-key`, NOT `Authorization: Bearer …`.
    // This is the most common provider-confusion bug; pin it explicitly.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-secret".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let api_key = reqs[0]
        .headers
        .get("x-api-key")
        .expect("x-api-key header required");
    assert_eq!(api_key, "sk-ant-secret");
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "Anthropic must NOT send Authorization header"
    );
}

#[tokio::test]
async fn chat_sends_anthropic_version_header() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let version = reqs[0]
        .headers
        .get("anthropic-version")
        .expect("anthropic-version header required on every request");
    // Don't pin the exact date — that's an implementation detail of
    // the constant ANTHROPIC_API_VERSION. Just assert it looks like a date.
    let v = version.to_str().unwrap();
    assert!(
        v.len() >= 10 && v.chars().nth(4) == Some('-'),
        "version must look like YYYY-MM-DD, got: {v}"
    );
}

#[tokio::test]
async fn chat_returns_error_on_5xx_with_status_in_message() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*/v1/messages$",
            503,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"upstream"}}"#,
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("5xx must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("503"), "error must mention status: {msg}");
    assert!(msg.contains("overloaded_error"), "error must include body");
}

#[tokio::test]
async fn chat_returns_error_on_401_invalid_api_key() {
    // Regression guard: pins current 'no-retry-on-4xx' behavior.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_status(
            "POST",
            r".*/v1/messages$",
            401,
            r#"{"type":"error","error":{"type":"authentication_error"}}"#,
        )
        .await;

    let provider = AnthropicProvider::new("bad".into(), Some(&server.url()));
    let err = provider
        .chat(&[user_msg("hi")], &[], &settings())
        .await
        .expect_err("401 must surface as Err");

    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "error must mention status: {msg}");
}

/// The request carries three breakpoints: last tool, system, and the last
/// block of the last message, so the conversation prefix is cached too.
fn assert_conversation_breakpoint(body: &Value) {
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

    let messages = body["messages"].as_array().unwrap();
    let last = messages.last().unwrap()["content"].as_array().unwrap();
    assert_eq!(last.last().unwrap()["cache_control"]["type"], "ephemeral");
    assert_eq!(last.last().unwrap()["type"], "tool_result");

    // Nothing else in the messages is marked.
    let marked = serde_json::to_string(messages)
        .unwrap()
        .matches("cache_control")
        .count();
    assert_eq!(marked, 1, "only the last block of the last message");
}

fn cache_test_conversation() -> Vec<ChatMessage> {
    let mut result = ChatMessage::text("tool", "file contents");
    result.tool_call_id = Some("toolu_1".into());
    vec![
        ChatMessage::text("system", "You are Koda."),
        user_msg("read it"),
        result,
    ]
}

fn cache_test_tools() -> Vec<koda_core::providers::ToolDefinition> {
    vec![koda_core::providers::ToolDefinition {
        name: "Read".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object"}),
    }]
}

#[tokio::test]
async fn chat_marks_the_last_message_block_as_a_cache_breakpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_ok("POST", r".*/v1/messages$", ok_messages_body())
        .await;

    let provider = AnthropicProvider::new("sk-ant-test".into(), Some(&server.url()));
    provider
        .chat(&cache_test_conversation(), &cache_test_tools(), &settings())
        .await
        .unwrap();

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_conversation_breakpoint(&body);
}

#[tokio::test]
async fn chat_stream_marks_the_last_message_block_as_a_cache_breakpoint() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":1}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
    let mut collector = provider
        .chat_stream(&cache_test_conversation(), &cache_test_tools(), &settings())
        .await
        .unwrap();
    while collector.rx.recv().await.is_some() {}

    let reqs = server.received_requests().await;
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_conversation_breakpoint(&body);
}

// ── chat_stream() ─────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_consumes_anthropic_sse_event_format_via_real_tcp() {
    // Anthropic SSE uses a different chunk shape than OpenAI:
    // each event has a `type` field and `content_block_delta` events
    // carry text in `delta.text`.
    let server = FakeLlmServer::spawn().await;
    server
        .mount_sse(
            "POST",
            r".*/v1/messages$",
            &[
                r#"{"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo "}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":3}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        )
        .await;

    let provider = AnthropicProvider::new("k".into(), Some(&server.url()));
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
