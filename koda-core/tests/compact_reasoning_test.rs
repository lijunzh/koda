//! Compaction with a reasoning model.
//!
//! A reasoning model spends the summary call's `max_tokens` on thinking
//! first. With a fixed 4096 cap it could use all of it and return no text
//! (`finish_reason: "length"`), compaction failed, and after three failures
//! the circuit breaker let requests go out far past the window.
//!
//! `compact::CONSECUTIVE_FAILURES` is process-global and an empty summary
//! records a failure, so every test here holds [`COMPACT_SERIAL`].

use koda_core::compact::{
    self, CompactResult, CompactSkip, is_compact_circuit_broken, record_compact_failure,
    reset_compact_failures,
};
use koda_core::engine::EngineEvent;
use koda_core::persistence::Persistence;
use koda_core::persistence::Role;
use koda_test_utils::{Env, MockProvider, MockResponse};
use std::sync::LazyLock;
use tokio::sync::{Mutex, MutexGuard};

static COMPACT_SERIAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

async fn lock_compact_state() -> MutexGuard<'static, ()> {
    let guard = COMPACT_SERIAL.lock().await;
    reset_compact_failures();
    guard
}

/// Twenty short messages, enough for a partial compaction (oldest 10 go).
async fn env_with_history(max_context_tokens: usize) -> Env {
    let env = Env::builder()
        .max_context_tokens(max_context_tokens)
        .build()
        .await;
    for i in 0..10 {
        env.insert_message(&Role::User, &format!("question {i}"))
            .await;
        env.insert_message(&Role::Assistant, &format!("answer {i}"))
            .await;
    }
    env
}

async fn compact_with(env: &Env, provider: &MockProvider) -> CompactResult {
    compact::compact_session_with_provider(
        &env.db,
        &env.session_id,
        env.config.max_context_tokens,
        &env.config.model_settings,
        provider,
    )
    .await
    .unwrap()
    .unwrap()
}

async fn history_text(env: &Env) -> String {
    let history = env.db.load_context(&env.session_id).await.unwrap();
    history
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn compact_summarizes_with_room_for_thinking() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<analysis>scratch</analysis><summary>The user asked ten questions.</summary>".into(),
    )]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary);
    assert_eq!(
        provider.recorded_max_tokens(),
        vec![Some(32_768)],
        "one call, capped at the window-sized budget instead of a fixed 4096"
    );
    let text = history_text(&env).await;
    assert!(text.contains("The user asked ten questions."));
    assert!(!text.contains("scratch"));
    assert!(!is_compact_circuit_broken());
}

#[tokio::test]
async fn compact_retries_an_empty_length_reply_with_a_larger_cap() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    // Reasoning used the whole cap: no text, stop reason "max_tokens".
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens(String::new()),
        MockResponse::Text("<summary>Recovered summary.</summary>".into()),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary);
    assert_eq!(
        provider.recorded_max_tokens(),
        vec![Some(32_768), Some(65_536)]
    );
    assert!(history_text(&env).await.contains("Recovered summary."));
    assert!(!is_compact_circuit_broken());
}

#[tokio::test]
async fn compact_leaves_a_marker_when_the_retry_is_empty_too() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let before = env.db.load_context(&env.session_id).await.unwrap().len();
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens(String::new()),
        MockResponse::TextMaxTokens(String::new()),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(result.no_summary);
    assert_eq!(
        provider.recorded_max_tokens().len(),
        2,
        "one retry, no more"
    );
    let after = env.db.load_context(&env.session_id).await.unwrap();
    assert!(
        after.len() < before,
        "history must shrink: {before} -> {}",
        after.len()
    );
    let text = history_text(&env).await;
    assert!(text.contains("(no summary available)"));
    assert!(!text.contains("answer 0"), "compacted answers must be gone");
    assert!(text.contains("question 0"), "the user's words are carried");
    assert!(text.contains("answer 9"), "the recent tail stays verbatim");
}

#[tokio::test]
async fn compact_retries_a_cut_off_reply_that_has_partial_text() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    // Flint, 2026-10-29: the reply hit the cap inside its <analysis> block,
    // 1,644 chars of preamble and no <summary>. That is not a summary.
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens("<analysis>\nThe conversation so far contains".into()),
        MockResponse::Text(
            "<analysis>done</analysis><summary>Recovered after the cut.</summary>".into(),
        ),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary);
    assert_eq!(
        provider.recorded_max_tokens(),
        vec![Some(32_768), Some(65_536)]
    );
    let text = history_text(&env).await;
    assert!(text.contains("Recovered after the cut."));
    assert!(
        !text.contains("conversation so far contains"),
        "the preamble is not stored"
    );
    assert!(!is_compact_circuit_broken());
}

#[tokio::test]
async fn compact_stores_the_marker_when_the_retry_is_still_a_preamble() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens("<analysis>\nstill thinking".into()),
        MockResponse::TextMaxTokens("<analysis>\nstill thinking, longer".into()),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(result.no_summary);
    assert_eq!(provider.recorded_max_tokens().len(), 2);
    let text = history_text(&env).await;
    assert!(text.contains("(no summary available)"));
    assert!(
        !text.contains("still thinking"),
        "an unclosed analysis is never the summary"
    );
}

#[tokio::test]
async fn compact_keeps_a_cut_off_summary_when_no_room_is_left_to_retry() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    // Both replies stop at the cap inside the <summary> block.
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens("<analysis>a</analysis><summary>Half a summ".into()),
        MockResponse::TextMaxTokens(
            "<analysis>a</analysis><summary>Most of a summary, then".into(),
        ),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary, "partial text beats the marker");
    assert_eq!(provider.recorded_max_tokens().len(), 2);
    let text = history_text(&env).await;
    assert!(text.contains("Most of a summary, then"));
    assert!(!text.contains("<summary>"), "the open tag is not stored");
}

#[tokio::test]
async fn compact_accepts_plain_text_without_tags() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "The user asked ten questions and got ten answers.".into(),
    )]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary);
    assert_eq!(provider.recorded_max_tokens().len(), 1);
    assert!(history_text(&env).await.contains("got ten answers"));
}

#[tokio::test]
async fn compact_caps_the_first_call_by_the_window_when_it_is_small() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(20_000).await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<summary>Short window.</summary>".into(),
    )]);

    compact_with(&env, &provider).await;

    let cap = provider.recorded_max_tokens()[0].unwrap();
    assert!(
        cap < 32_768,
        "bounded by the room the window has left, got {cap}"
    );
    assert!(cap >= 1_024);
}

#[tokio::test]
async fn compact_does_not_retry_an_empty_reply_that_was_not_cut_off() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let provider = MockProvider::new(vec![MockResponse::Text(String::new())]);

    let result = compact_with(&env, &provider).await;

    assert!(result.no_summary);
    assert_eq!(provider.recorded_max_tokens().len(), 1);
}

/// Text the model would be sent, across every message in the context.
async fn context_text(env: &Env) -> String {
    env.db
        .load_context(&env.session_id)
        .await
        .unwrap()
        .iter()
        .filter_map(|m| m.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn env_with_task(task: &str) -> Env {
    let env = Env::builder().max_context_tokens(8_000).build().await;
    let padding = "x".repeat(2200);
    env.insert_message(&Role::User, task).await;
    for i in 0..7 {
        env.insert_message(&Role::Assistant, &format!("reply {i}: {padding}"))
            .await;
        env.insert_message(&Role::Tool, &format!("result {i}: {padding}"))
            .await;
    }
    env
}

#[tokio::test]
async fn dropping_without_a_summary_keeps_the_task_through_two_drops() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;

    for _ in 0..2 {
        let result = compact::compact_without_summary(&env.db, &env.session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(result.no_summary);
    }

    let text = context_text(&env).await;
    assert!(text.contains("Fix the parser in lexer.rs"), "{text}");
    for msg in env.db.load_context(&env.session_id).await.unwrap() {
        let row = msg.content.unwrap_or_default();
        assert!(row.matches("(no summary available)").count() <= 1, "{row}");
    }
}

#[tokio::test]
async fn an_empty_summary_keeps_the_task() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let provider = MockProvider::new(vec![
        MockResponse::TextMaxTokens(String::new()),
        MockResponse::TextMaxTokens(String::new()),
    ]);

    let result = compact_with(&env, &provider).await;

    assert!(result.no_summary);
    let text = context_text(&env).await;
    assert!(text.contains("Fix the parser in lexer.rs"), "{text}");
}

#[tokio::test]
async fn carried_text_stays_within_the_cap() {
    let _serial = lock_compact_state().await;
    let env = env_with_task(&"é".repeat(20_000)).await;

    let result = compact::compact_without_summary(&env.db, &env.session_id)
        .await
        .unwrap()
        .unwrap();

    assert!(result.summary_tokens < 1_100, "{}", result.summary_tokens);
}

#[tokio::test]
async fn compact_without_summary_skips_a_short_history() {
    let _serial = lock_compact_state().await;
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    let result = compact::compact_without_summary(&env.db, &env.session_id)
        .await
        .unwrap();

    assert!(matches!(result, Err(CompactSkip::TooShort(1))));
}

/// The scenario from the eval logs: summaries keep failing, the breaker has
/// tripped, and the history is past the window. The request must not go out
/// as it is.
#[tokio::test]
async fn preflight_shrinks_history_past_the_window_when_the_breaker_is_tripped() {
    let _serial = lock_compact_state().await;
    // 15 messages of ~2200 chars come to about 9,600 estimated tokens: 120%
    // of an 8,000-token window. Halving them lands near 64%.
    let env = Env::builder().max_context_tokens(8_000).build().await;
    let padding = "x".repeat(2200);
    for i in 0..7 {
        env.insert_message(&Role::User, &format!("user {i}: {padding}"))
            .await;
        env.insert_message(&Role::Assistant, &format!("reply {i}: {padding}"))
            .await;
    }
    env.insert_user_message("now answer the real question")
        .await;
    for _ in 0..3 {
        record_compact_failure();
    }
    assert!(is_compact_circuit_broken());

    let provider = MockProvider::new(vec![MockResponse::Text("the real answer".into())]);
    let (result, events) = env.run_inference_result(&provider).await;

    assert!(
        result.is_ok(),
        "inference should succeed: {:?}",
        result.err()
    );
    let sent = provider.recorded_calls();
    assert_eq!(
        sent.len(),
        1,
        "no summary call while the breaker is tripped"
    );
    let sent_chars: usize = sent[0]
        .iter()
        .filter_map(|m| m.content.as_deref())
        .map(str::len)
        .sum();
    assert!(
        sent_chars < 8_000 * 4,
        "request still carries {sent_chars} chars, past an 8,000-token window"
    );
    assert!(
        events.iter().any(|e| matches!(e,
            EngineEvent::Warn { message } if message.contains("without a summary"))),
        "the user should be told history was dropped; events: {events:?}"
    );
}

/// The signal the retry keys on: a non-streaming OpenAI-compatible reply cut
/// off by the token cap reports `max_tokens`, like the streaming path does.
#[tokio::test]
async fn openai_compat_chat_reports_length_as_max_tokens() {
    use koda_core::config::{ModelSettings, ProviderType};
    use koda_core::providers::openai_compat::OpenAiCompatProvider;
    use koda_core::providers::{ChatMessage, LlmProvider};
    use koda_test_utils::network::FakeLlmServer;

    let server = FakeLlmServer::spawn().await;
    server
        .mount_chat_ok(serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": "", "reasoning_content": "thinking"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4096}
        }))
        .await;

    let provider = OpenAiCompatProvider::new(&server.url(), None);
    let settings = ModelSettings::defaults_for("flint", &ProviderType::LMStudio);
    let response = provider
        .chat(&[ChatMessage::text("user", "hi")], &[], &settings)
        .await
        .unwrap();

    assert!(response.content.is_none());
    assert_eq!(response.usage.stop_reason, "max_tokens");
}
