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

/// A summary's row sorts after the tail it was stored with. The next
/// compaction must still see it first, or it summarizes the tail with no
/// request in sight and says the request is unknown.
#[tokio::test]
async fn the_next_compaction_reads_the_last_summary_and_replaces_it() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let provider = MockProvider::new(vec![
        MockResponse::Text("<summary>The task is fixing lexer.rs. MIDDLE END</summary>".into()),
        MockResponse::Text("<summary>Second summary.</summary>".into()),
    ]);

    compact_with(&env, &provider).await;
    for i in 0..10 {
        env.insert_message(&Role::Assistant, &format!("later answer {i}"))
            .await;
        env.insert_message(&Role::User, &format!("later question {i}"))
            .await;
    }
    compact_with(&env, &provider).await;

    let prompts = provider.recorded_calls();
    let second = prompts[1][0].content.as_deref().unwrap();
    let conversation = second.split("---\n\n").last().unwrap();
    assert!(
        conversation.starts_with("[system]: [Compacted conversation summary]"),
        "{conversation}"
    );
    assert!(conversation.contains("MIDDLE END"), "{conversation}");

    let history = env.db.load_context(&env.session_id).await.unwrap();
    let summaries: Vec<_> = history
        .iter()
        .filter(|m| m.role == Role::System)
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert_eq!(summaries.len(), 1, "{summaries:?}");
    assert!(summaries[0].contains("Second summary."), "{summaries:?}");
}

/// A summary can drop or rewrite the task. The user's own words go past it.
#[tokio::test]
async fn the_task_survives_summaries_that_lose_it() {
    let _serial = lock_compact_state().await;
    let task = "Fix the parser in lexer.rs";
    let env = env_with_task(task).await;
    let provider = MockProvider::new(vec![
        MockResponse::Text("<summary>Some code was read.</summary>".into()),
        MockResponse::Text("<summary>More code was read.</summary>".into()),
    ]);

    compact_with(&env, &provider).await;
    for i in 0..10 {
        env.insert_message(&Role::Assistant, &format!("later answer {i}"))
            .await;
        env.insert_message(&Role::User, &format!("later question {i}"))
            .await;
    }
    compact_with(&env, &provider).await;

    let text = context_text(&env).await;
    assert_eq!(text.matches(task).count(), 1, "{text}");
    assert!(text.contains("More code was read."), "{text}");
    assert!(!text.contains("Some code was read."), "{text}");
}

/// The summary holds the only copy of what it replaced, so it is neither
/// clipped like a tool result nor the first row to go when the text is too big.
#[tokio::test]
async fn an_oversized_compaction_keeps_the_last_summary_whole() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let long_summary = format!("{}TAIL OF THE SUMMARY", "detail ".repeat(400));
    let provider = MockProvider::new(vec![
        MockResponse::Text(format!("<summary>{long_summary}</summary>")),
        MockResponse::Text("<summary>Second summary.</summary>".into()),
    ]);
    compact_with(&env, &provider).await;

    for i in 0..30 {
        env.insert_message(&Role::Assistant, &"a".repeat(1_900))
            .await;
        env.insert_message(&Role::User, &format!("later question {i}"))
            .await;
    }
    let result = compact::compact_session_with_provider(
        &env.db,
        &env.session_id,
        12_000,
        &env.config.model_settings,
        &provider,
    )
    .await
    .unwrap();
    assert!(result.is_ok(), "the compaction must run");

    let prompts = provider.recorded_calls();
    let second = prompts[1][0].content.as_deref().unwrap();
    assert!(second.contains("TAIL OF THE SUMMARY"));
    assert!(
        second.len() < 33_000,
        "the conversation was meant to be truncated: {} chars",
        second.len()
    );
}

/// A run stopped reading code and asked for permission the user had already
/// given after a summary told it not to edit.
#[tokio::test]
async fn the_summary_prompt_forbids_invented_restrictions() {
    let _serial = lock_compact_state().await;
    let env = env_with_history(100_000).await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<summary>Handed over.</summary>".into(),
    )]);

    compact_with(&env, &provider).await;

    let prompts = provider.recorded_calls();
    let prompt = prompts[0][0].content.as_deref().unwrap();
    assert!(prompt.contains("Add no instruction"), "{prompt}");
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

async fn insert_tool_batch(env: &Env, batch: usize) {
    let calls: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            serde_json::json!({
                "id": format!("call_{batch}_{i}"),
                "function_name": "Read",
                "arguments": "{}",
            })
        })
        .collect();
    let id = env
        .db
        .insert_message(
            &env.session_id,
            &Role::Assistant,
            None,
            Some(&serde_json::to_string(&calls).unwrap()),
            None,
            None,
        )
        .await
        .unwrap();
    env.db.mark_message_complete(id).await.unwrap();
    for i in 0..2 {
        insert_tool_result(
            env,
            &format!("call_{batch}_{i}"),
            &format!("output {batch} {i}"),
        )
        .await;
    }
}

async fn insert_tool_result(env: &Env, call_id: &str, text: &str) {
    env.db
        .insert_message(
            &env.session_id,
            &Role::Tool,
            Some(text),
            None,
            Some(call_id),
            None,
        )
        .await
        .unwrap();
}

/// Each of `texts` went to the summary or stayed in the context, none lost.
async fn assert_each_was_summarized_or_kept(env: &Env, provider: &MockProvider, texts: &[String]) {
    let calls = provider.recorded_calls();
    let prompt = calls[0][0].content.clone().unwrap_or_default();
    let kept = context_text(env).await;
    for text in texts {
        assert!(
            prompt.contains(text) || kept.contains(text),
            "{text} is in neither the summary prompt nor the context"
        );
    }
}

/// Keeping the newest 4 of these 10 rows would start the tail on the second
/// result of batch 1, separated from its call.
#[tokio::test]
async fn compaction_never_splits_a_tool_batch() {
    let _serial = lock_compact_state().await;
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_message(&Role::User, "Fix the parser").await;
    for batch in 0..3 {
        insert_tool_batch(&env, batch).await;
    }
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<summary>Read three batches of files.</summary>".into(),
    )]);

    compact_with(&env, &provider).await;

    let texts: Vec<String> = (0..3)
        .flat_map(|batch| (0..2).map(move |i| format!("output {batch} {i}")))
        .collect();
    assert_each_was_summarized_or_kept(&env, &provider, &texts).await;
}

/// A short tail made of one batch stays whole rather than being compacted away.
#[tokio::test]
async fn compaction_keeps_a_batch_that_fills_the_tail() {
    let _serial = lock_compact_state().await;
    let env = Env::builder().max_context_tokens(100_000).build().await;
    env.insert_message(&Role::User, "Fix the parser").await;
    env.insert_message(&Role::Assistant, "Reading.").await;
    insert_tool_batch(&env, 0).await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<summary>The user wants the parser fixed.</summary>".into(),
    )]);

    compact_with(&env, &provider).await;

    let text = context_text(&env).await;
    assert!(text.contains("output 0 0"), "{text}");
    assert!(text.contains("output 0 1"), "{text}");
}

/// Rows pruned at load count toward the raw table but not the history, so the
/// two must agree on where the tail starts.
#[tokio::test]
async fn compaction_counts_the_same_rows_it_was_shown() {
    let _serial = lock_compact_state().await;
    let env = Env::builder().max_context_tokens(100_000).build().await;
    let mut texts = Vec::new();
    for i in 0..10 {
        for (role, text) in [
            (Role::User, format!("question {i}")),
            (Role::Assistant, format!("answer {i}")),
        ] {
            env.insert_message(&role, &text).await;
            texts.push(text);
        }
        if i >= 5 {
            insert_tool_result(&env, &format!("orphan_{i}"), "pruned at load").await;
        }
    }
    env.insert_message(&Role::User, "last question").await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<summary>The user asked ten questions.</summary>".into(),
    )]);

    compact_with(&env, &provider).await;

    assert_each_was_summarized_or_kept(&env, &provider, &texts).await;
}

/// Gemini, 2026-11-01: section 6 of the summary quotes the user's messages,
/// and the summary prompt names the tag. Taking the last `<summary>` kept only
/// what followed the quote, and the task went with the rest.
#[tokio::test]
async fn a_tag_quoted_inside_the_summary_does_not_cut_it() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let reply = "<analysis>\nthinking\n</analysis>\n\n<summary>\n\
                 1. Primary Request: fix the parser.\n\
                 6. User messages: \"an <analysis> block followed by a <summary> block\"\n\
                 7. Pending Tasks: none.\n</summary>";
    let provider = MockProvider::new(vec![MockResponse::Text(reply.into())]);

    compact_with(&env, &provider).await;

    let text = context_text(&env).await;
    assert!(text.contains("Primary Request: fix the parser"), "{text}");
    assert!(text.contains("Pending Tasks: none"), "{text}");
    assert!(!text.contains("thinking"), "{text}");
}

/// Gemini, 2026-11-01: replies of 8-11K chars stopped normally and still came
/// back as no summary, after the fix for a missing tag.
#[tokio::test]
async fn a_summary_inside_the_analysis_or_after_a_quoted_tag_is_found() {
    for reply in [
        "<analysis>thinking<summary>The real summary.</summary></analysis>",
        "<analysis>the code closes </analysis> early here, then more</analysis>\
         <summary>The real summary.</summary>",
        "<analysis>it looks for <summary> and </summary> tags</analysis>\
         <summary>The real summary.</summary>",
    ] {
        let _serial = lock_compact_state().await;
        let env = env_with_task("Fix the parser in lexer.rs").await;
        let provider = MockProvider::new(vec![MockResponse::Text(reply.into())]);

        let result = compact_with(&env, &provider).await;

        assert!(!result.no_summary, "{reply}");
        assert!(context_text(&env).await.contains("The real summary."));
        assert!(!context_text(&env).await.contains("thinking"), "{reply}");
    }
}

/// Gemini, 2026-10-31: three replies of 6-9K chars stopped normally with no
/// <summary> tag, and all of that text was thrown away for the marker.
#[tokio::test]
async fn a_finished_reply_with_no_summary_tag_keeps_its_analysis() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let provider = MockProvider::new(vec![MockResponse::Text(
        "<analysis>The user wants the parser fixed in lexer.rs.</analysis>".into(),
    )]);

    let result = compact_with(&env, &provider).await;

    assert!(!result.no_summary);
    assert_eq!(provider.recorded_max_tokens().len(), 1, "no retry needed");
    let text = context_text(&env).await;
    assert!(text.contains("The user wants the parser fixed"), "{text}");
    assert!(!text.contains("<analysis>"), "{text}");
}

/// Gemini, 2026-10-31: a real run stored a summary of under four characters
/// and the task went with the messages it replaced.
#[tokio::test]
async fn a_summary_too_short_to_mean_anything_keeps_the_task() {
    let _serial = lock_compact_state().await;
    let env = env_with_task("Fix the parser in lexer.rs").await;
    let provider = MockProvider::new(vec![MockResponse::Text("OK".into())]);

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
        events
            .iter()
            .filter_map(EngineEvent::notice)
            .any(|e| matches!(e,
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
