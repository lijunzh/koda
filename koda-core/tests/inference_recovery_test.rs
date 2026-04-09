//! Tests for inference loop error recovery paths.
//!
//! Exercises rate-limit retry (429 → backoff → success) and context-overflow
//! recovery (overflow → compact → retry → success).

use koda_core::{engine::EngineEvent, persistence::Persistence};
use koda_test_utils::{Env, MockProvider, MockResponse, Role};

// ── Rate limit retry tests ───────────────────────────────────

#[tokio::test]
async fn test_rate_limit_single_retry_recovers() {
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_user_message("hello").await;

    // First call: 429, second call: success
    let provider = MockProvider::new(vec![
        MockResponse::RateLimit,
        MockResponse::Text("recovered after rate limit".into()),
    ]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(result.is_ok(), "should recover: {:?}", result.err());

    // Should have a warning about rate limiting
    let has_rate_warn = events.iter().any(|e| {
        matches!(
            e,
            EngineEvent::Warn { message } if message.contains("Rate limited")
        )
    });
    assert!(has_rate_warn, "expected rate limit warning in events");

    // Response should be persisted
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("recovered after rate limit"),
        "DB should contain recovered response: {last}"
    );
}

#[tokio::test]
async fn test_rate_limit_exhausted_returns_error() {
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_user_message("hello").await;

    // All 5 retries fail with rate limit
    let provider = MockProvider::new(vec![
        MockResponse::RateLimit,
        MockResponse::RateLimit,
        MockResponse::RateLimit,
        MockResponse::RateLimit,
        MockResponse::RateLimit,
    ]);

    let (result, _events) = env.run_inference_result(&provider).await;
    assert!(result.is_err(), "should fail after exhausting retries");
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("429") || err.contains("Too Many Requests"),
        "error should mention rate limit: {err}"
    );
}

// ── Context overflow recovery tests ──────────────────────────

#[tokio::test]
async fn test_context_overflow_compacts_and_retries() {
    let env = Env::builder().max_context_tokens(100_000).build().await;

    // Need >= 4 messages in history for compaction to proceed.
    env.insert_message(&Role::User, "first question").await;
    env.insert_message(&Role::Assistant, "first answer").await;
    env.insert_message(&Role::User, "second question").await;
    env.insert_message(&Role::Assistant, "second answer").await;
    env.insert_message(&Role::User, "third question that overflows")
        .await;

    // Response sequence:
    // 1. chat_stream() → ContextOverflow (triggers recovery)
    // 2. chat() → compaction summary (non-streaming)
    // 3. chat_stream() → success (retry after compaction)
    let provider = MockProvider::new(vec![
        MockResponse::ContextOverflow,
        MockResponse::Text("Summary: user asked three questions.".into()),
        MockResponse::Text("recovered after compaction".into()),
    ]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(result.is_ok(), "should recover: {:?}", result.err());

    // Should have a warning about context overflow
    let has_overflow_warn = events.iter().any(|e| {
        matches!(
            e,
            EngineEvent::Warn { message } if message.contains("context overflow")
                || message.contains("Context overflow")
                || message.contains("overflow")
        )
    });
    assert!(
        has_overflow_warn,
        "expected overflow warning in events: {events:?}"
    );

    // Should have compaction info
    let has_compact_info = events.iter().any(|e| {
        matches!(
            e,
            EngineEvent::Info { message } if message.contains("Compacted")
        )
    });
    assert!(
        has_compact_info,
        "expected compaction info in events: {events:?}"
    );

    // Response should be persisted
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("recovered after compaction"),
        "DB should contain recovered response: {last}"
    );
}

#[tokio::test]
async fn test_context_overflow_too_few_messages_fails() {
    let env = Env::builder().max_context_tokens(100_000).build().await;

    // Only 1 message — compaction will skip (TooShort), recovery fails.
    env.insert_user_message("hello").await;

    let provider = MockProvider::new(vec![MockResponse::ContextOverflow]);

    let (result, _events) = env.run_inference_result(&provider).await;
    assert!(result.is_err(), "should fail when compaction can't help");
    let err = format!("{:#}", result.unwrap_err());
    assert!(
        err.contains("context overflow") || err.contains("too long"),
        "error should mention overflow: {err}"
    );
}
