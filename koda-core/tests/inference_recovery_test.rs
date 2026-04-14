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

// ── Network error clean resume tests (#875, #877) ───────────────────────

/// After a network error, the next inference turn should see only the
/// original user message — no partial responses, no sentinel messages.
/// This verifies that `load_context()` filters incomplete assistant
/// messages (those without `completed_at`).
#[tokio::test]
async fn test_network_error_next_turn_gets_clean_context() {
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_user_message("research the inference loop").await;

    // First call: network error after partial text
    let provider = MockProvider::new(vec![MockResponse::NetworkError {
        partial_text: "I'll start by".into(),
        error: "connection reset".into(),
    }]);

    let (result, events) = env.run_inference_result(&provider).await;
    assert!(
        result.is_ok(),
        "network error should not propagate: {result:?}"
    );

    let has_network_warn = events.iter().any(|e| {
        matches!(e, EngineEvent::Warn { message } if message.contains("network") || message.contains("Network") || message.contains("connection"))
    });
    assert!(
        has_network_warn,
        "expected network warning; events: {events:?}"
    );

    // Now simulate the user typing "continue" — second turn
    env.insert_user_message("continue").await;

    let provider2 = MockProvider::new(vec![MockResponse::Text(
        "Here is the full research on the inference loop.".into(),
    )]);

    let _events2 = env.run_inference(&provider2).await;

    // The recorded call should contain the original user message, the
    // "continue" message, and NO partial response or sentinel.
    let calls = provider2.recorded_calls();
    assert!(!calls.is_empty(), "provider should have been called");
    let messages = &calls[0];

    // No message should contain the partial text "I'll start by"
    for msg in messages {
        if let Some(ref content) = msg.content {
            assert!(
                !content.contains("I'll start by"),
                "partial response from network error should NOT be in context: {content}"
            );
            assert!(
                !content.contains("[System]"),
                "no sentinel system messd be in context: {content}"
            );
        }
    }

    // The original user message should be present
    let has_original = messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains("research the inference loop"))
    });
    assert!(has_original, "original user message should be in context");

    // The "continue" message should be present
    let has_continue = messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains("continue")));
    assert!(has_continue, "continue message should be in context");
}

/// After a Ctrl+C interruption, the partial assistant response is saved
/// but should be filtered from context on the next turn (since
/// `mark_message_complete` was never called).
#[tokio::test]
async fn test_ctrl_c_next_turn_excludes_partial_response() {
    use tokio_util::sync::CancellationToken;

    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_user_message("write a poem about cats").await;

    // Simulate: model streams text, then user hits Ctrl+C
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Use a normal text response but cancel immediately after starting
    let provider = MockProvider::new(vec![MockResponse::Text(
        "Whiskers twitch in moonlit night".into(),
    )]);

    // Cancel before inference even starts streaming — the cancellation
    // token will be checked during stream collection.
    cancel_clone.cancel();

    let (result, _events) = env.run_inference_cancellable(&provider, cancel).await;
    assert!(result.is_ok(), "cancellation should not error: {result:?}");

    // Next turn: user types something new
    env.insert_user_message("now write about dogs").await;

    let provider2 = MockProvider::new(vec![MockResponse::Text("Dogs are loyal and true.".into())]);

    let _events2 = env.run_inference(&provider2).await;

    let calls = provider2.recorded_calls();
    assert!(!calls.is_empty());
    let messages = &calls[0];

    // The original user message should be present
    let has_poem = messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains("poem about cats"))
    });
    assert!(has_poem, "original prompt should be in context");

    // The "dogs" message should be present
    let has_dogs = messages.iter().any(|m| {
        m.content
            .as_deref()
            .is_some_and(|c| c.contains("about dogs"))
    });
    assert!(has_dogs, "new prompt should be in context");

    // No incomplete assistant response should be in context
    // (it wasn't marked complete, so load_context filters it)
    let assistant_msgs: Vec<_> = messages.iter().filter(|m| m.role == "assistant").collect();
    for m in &assistant_msgs {
        if let Some(ref content) = m.content {
            assert!(
                !content.contains("Whiskers"),
                "incomplete response should be filtered from context: {content}"
            );
        }
    }
}
