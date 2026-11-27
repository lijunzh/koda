//! End-to-end tests: core pipeline.
//!
//! Mock provider → inference loop → real tools → DB persistence.
//! All file operations happen in isolated temp directories.

use koda_core::engine::EngineEvent;
use koda_state::session_store::SessionStore;
use koda_test_utils::{Env, MockProvider, MockResponse};

// ── Core pipeline tests ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_text_response_streams_and_persists() {
    let env = Env::new().await;
    env.insert_user_message("say hello").await;

    let provider = MockProvider::new(vec![MockResponse::Text("Hello, world!".into())]);
    let events = env.run_inference(&provider).await;

    let text_deltas: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::TextDelta { .. }))
        .collect();
    assert!(!text_deltas.is_empty(), "expected TextDelta events");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TextDone { .. })),
        "expected TextDone"
    );

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Hello, world!"),
        "DB should contain response: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tool_call_executes_and_returns() {
    let env = Env::new().await;
    env.insert_user_message("run echo").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "echo hello"})),
        MockResponse::Text("Done! The command printed hello.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallStart { name, .. } if name == "Bash")),
        "expected ToolCallStart for Bash"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Bash")),
        "expected ToolCallResult for Bash"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::TextDone { .. })),
        "expected TextDone after tool execution"
    );

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Done!"),
        "DB should contain final response: {last}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_read_tool_in_sandbox() {
    let env = Env::new().await;
    let test_file = env.root.join("test_data.txt");
    std::fs::write(&test_file, "sandbox content here").unwrap();

    env.insert_user_message("read the file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Read",
            serde_json::json!({"path": test_file.to_string_lossy()}),
        ),
        MockResponse::Text("The file contains sandbox content.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "Read"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(
        tool_result.is_some(),
        "expected Read tool result in events: {events:?}"
    );
    assert!(
        tool_result.unwrap().contains("sandbox content here"),
        "Read tool should return file content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_write_tool_creates_file_in_sandbox() {
    let env = Env::new().await;
    env.insert_user_message("create a file").await;

    let target = env.root.join("created.txt");
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "hello from mock"
            }),
        ),
        MockResponse::Text("File created.".into()),
    ]);
    env.run_inference(&provider).await;

    assert!(target.exists(), "Write tool should create the file");
    let content = std::fs::read_to_string(&target).unwrap();
    assert_eq!(content, "hello from mock");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_session_history_persists_across_turns() {
    let env = Env::new().await;

    // Turn 1
    env.insert_user_message("first question").await;
    let provider1 = MockProvider::new(vec![MockResponse::Text("first answer".into())]);
    env.run_inference(&provider1).await;

    // Turn 2
    env.insert_user_message("second question").await;
    let provider2 = MockProvider::new(vec![MockResponse::Text("second answer".into())]);
    env.run_inference(&provider2).await;

    let messages = env.db.load_context(&env.session_id).await.unwrap();
    let contents: Vec<String> = messages.iter().filter_map(|m| m.content.clone()).collect();

    assert!(contents.iter().any(|c| c.contains("first question")));
    assert!(contents.iter().any(|c| c.contains("first answer")));
    assert!(contents.iter().any(|c| c.contains("second question")));
    assert!(contents.iter().any(|c| c.contains("second answer")));
}

/// Koda is built for the current Claude models: an older one runs as if it
/// were current, with a warning on its first turn, and only the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_claude_model_is_warned_about_once() {
    let mut env = Env::builder()
        .provider_type(koda_core::config::ProviderType::Anthropic)
        .build()
        .await;
    let warnings = |events: &[EngineEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Warn { message } if message.contains("older Claude model") => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect()
    };

    env.config.model = "claude-sonnet-5-5".into();
    env.insert_user_message("hi").await;
    let events = env
        .run_inference(&MockProvider::new(vec![MockResponse::Text("Hi.".into())]))
        .await;
    assert!(warnings(&events).is_empty(), "a current model");

    // A model nothing else in the suite uses, so "once" is this test's.
    env.config.model = "claude-opus-4-1-20250805".into();
    for prompt in ["again", "and again"] {
        env.insert_user_message(prompt).await;
        let events = env
            .run_inference(&MockProvider::new(vec![MockResponse::Text("Hi.".into())]))
            .await;
        let found = warnings(&events);
        if prompt == "again" {
            assert_eq!(found.len(), 1, "{events:?}");
            assert!(found[0].starts_with("claude-opus-4-1-20250805 is an older"));
        } else {
            assert!(found.is_empty(), "warned twice");
        }
    }
}
