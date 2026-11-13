//! Golden-file replay tests.
//!
//! Demonstrates replaying recorded LLM conversations from JSON fixtures.
//! To record new golden files, wrap a real provider with `RecordingProvider`.
//!
//! Run: `cargo test -p koda-core --features test-support --test golden_test`

use koda_core::engine::EngineEvent;
use koda_test_utils::{Env, golden};

/// Regression test for #773 → #823: Gemma 4 emitting 10 identical List calls
/// in a single response. With dedup removed (#823), all calls execute.
/// If a model is broken enough to emit duplicates, the user should see it
/// and switch models rather than us silently papering over it.
#[tokio::test]
async fn gemma4_duplicate_list_calls_all_execute() {
    let provider = golden::replay("tests/fixtures/gemma4_duplicate_list.golden.json");
    let env = Env::new().await;
    env.insert_user_message("ls").await;

    let events = env.run_inference(&provider).await;

    // No dedup warning should appear
    let dedup_warn = events.iter().any(|e| {
        matches!(
            e,
            EngineEvent::Warn { message } if message.contains("duplicate")
        )
    });
    assert!(!dedup_warn, "should NOT see dedup warning: {events:?}");

    // All 10 List calls should execute (no dedup)
    let list_calls: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallStart { name, .. } if name == "List"))
        .collect();
    assert_eq!(
        list_calls.len(),
        10,
        "expected 10 List calls (no dedup), got {}: {list_calls:?}",
        list_calls.len(),
    );
}

#[tokio::test]
async fn replay_example_conversation() {
    let provider = golden::replay("tests/fixtures/example_conversation.golden.json");
    let env = Env::new().await;

    // Create the file the golden fixture's Read tool call expects.
    std::fs::write(env.root.join("README.md"), "# Koda\nA coding agent.").unwrap();

    // The golden file has 2 responses:
    // 1. Tool call (Read README.md)
    // 2. Text summary after reading the file
    env.insert_user_message("read the README and summarize it")
        .await;

    let events = env.run_inference(&provider).await;

    // Collect all text deltas
    let all_text: String = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    assert!(
        all_text.contains("README"),
        "expected text about README, got: {all_text:?}"
    );
}
