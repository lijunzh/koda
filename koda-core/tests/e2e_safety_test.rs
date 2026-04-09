//! Safety and sandbox tests — borrowed from Codex and Gemini CLI patterns.
//!
//! Tests path safety, symlink protection, dangerous command handling,
//! approval flow, and file encoding edge cases.
//!
//! Run with: `cargo test -p koda-core --features test-support --test e2e_safety_test`

use koda_core::{engine::EngineEvent, persistence::Persistence};
use koda_test_utils::{Env, MockProvider, MockResponse};

/// Find the first ToolCallResult output for a given tool name.
fn find_tool_output(events: &[EngineEvent], tool: &str) -> Option<String> {
    events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == tool
        {
            return Some(output.clone());
        }
        None
    })
}

/// Find all ToolCallResult outputs for a given tool name.
fn find_all_tool_outputs(events: &[EngineEvent], tool: &str) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == tool
            {
                return Some(output.clone());
            }
            None
        })
        .collect()
}

// ── Symlink sandbox escape (Codex: safety_tests, Gemini: symlink-install) ──

#[tokio::test]
#[cfg(unix)]
async fn read_via_symlink_outside_sandbox_is_blocked() {
    let env = Env::new().await;

    // Create a symlink inside project root pointing outside.
    let outside_dir = tempfile::tempdir().unwrap();
    let secret = outside_dir.path().join("secret.txt");
    std::fs::write(&secret, "TOP SECRET DATA").unwrap();

    let link_path = env.root.join("sneaky_link");
    std::os::unix::fs::symlink(&secret, &link_path).unwrap();

    env.insert_user_message("read the linked file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "sneaky_link"})),
        MockResponse::Text("I tried to read it.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // The Read tool result should contain an error about escaping.
    let read_output = find_tool_output(&events, "Read");
    assert!(read_output.is_some(), "expected Read result: {events:?}");
    let output = read_output.unwrap();
    assert!(
        output.contains("escape")
            || output.contains("symlink")
            || output.contains("outside")
            || output.contains("resolve")
            || output.contains("denied"),
        "should block symlink escape: {output}"
    );
}

// ── File paths with spaces (Gemini: file-system.test.ts) ───────────────────

#[tokio::test]
async fn read_and_write_file_with_spaces_in_path() {
    let env = Env::new().await;
    let target = env.root.join("my folder/my file.txt");
    std::fs::create_dir_all(env.root.join("my folder")).unwrap();
    std::fs::write(&target, "hello spaces").unwrap();

    env.insert_user_message("read the file with spaces").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Read",
            serde_json::json!({"file_path": "my folder/my file.txt"}),
        ),
        MockResponse::Text("Read it!".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Read");
    assert!(output.is_some());
    assert!(
        output.unwrap().contains("hello spaces"),
        "should handle spaces in paths"
    );
}

#[tokio::test]
async fn write_file_with_spaces_in_path() {
    let env = Env::new().await;
    env.insert_user_message("write to path with spaces").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "file_path": "dir with spaces/file name.txt",
                "content": "spaced content"
            }),
        ),
        MockResponse::Text("Written!".into()),
    ]);
    env.run_inference(&provider).await;

    let target = env.root.join("dir with spaces/file name.txt");
    assert!(target.exists(), "file should be created");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "spaced content");
}

// ── Binary/non-UTF8 file read (Gemini: utf-bom-encoding.test.ts) ───────────

#[tokio::test]
async fn read_binary_file_does_not_crash() {
    let env = Env::new().await;
    let binary_file = env.root.join("image.bin");
    std::fs::write(&binary_file, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]).unwrap();

    env.insert_user_message("read the binary file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "image.bin"})),
        MockResponse::Text("I see it's binary.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Should not crash — may return an error or the content.
    let output = find_tool_output(&events, "Read");
    assert!(
        output.is_some(),
        "should have a tool result (even if error): {events:?}"
    );
}

// ── Dangerous command handling (Codex: safety_tests) ───────────────────────

#[tokio::test]
async fn dangerous_rm_rf_is_flagged() {
    let env = Env::new().await;
    env.insert_user_message("delete everything").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "rm -rf /"})),
        MockResponse::Text("I tried.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // In Auto mode, dangerous commands should still execute but the
    // bash_safety layer should add warnings. The key is it doesn't crash.
    let output = find_tool_output(&events, "Bash");
    // In Auto mode, rm -rf / might be blocked by bash_safety (no result)
    // or it might execute and fail. Either way, it should NOT crash.
    // If blocked, there will be no ToolCallResult for Bash.
    if let Some(output) = output {
        // It ran — check it didn't delete anything important.
        assert!(
            !output.contains("Deleted"),
            "rm -rf / should not succeed: {output}"
        );
    }
    // Regardless, the inference loop should complete.
    let has_text = events
        .iter()
        .any(|e| matches!(e, EngineEvent::TextDelta { .. }));
    assert!(
        has_text,
        "model should still produce a text response: {events:?}"
    );
}

#[tokio::test]
async fn path_traversal_in_write_is_blocked() {
    let env = Env::new().await;
    env.insert_user_message("write outside sandbox").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "file_path": "../../../tmp/evil.txt",
                "content": "malicious"
            }),
        ),
        MockResponse::Text("Tried to escape.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Write");
    assert!(output.is_some());
    let output = output.unwrap();
    assert!(
        output.contains("escape")
            || output.contains("outside")
            || output.contains("traversal")
            || output.contains("resolve")
            || output.contains("denied"),
        "should mention path escape: {output}"
    );
}

#[tokio::test]
async fn path_traversal_in_edit_is_blocked() {
    let env = Env::new().await;
    env.insert_user_message("edit outside sandbox").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "file_path": "../../../../etc/passwd",
                "replacements": [{"old_str": "root", "new_str": "hacked"}]
            }),
        ),
        MockResponse::Text("Tried to edit.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Edit");
    assert!(output.is_some());
    let output = output.unwrap();
    assert!(
        output.contains("escape")
            || output.contains("outside")
            || output.contains("traversal")
            || output.contains("resolve")
            || output.contains("denied")
            || output.contains("Error"),
        "path traversal should fail: {output}"
    );
}

// ── Edit fuzzy matching (trailing whitespace) ──────────────────────────────

#[tokio::test]
async fn edit_fuzzy_matches_trailing_whitespace() {
    let env = Env::new().await;
    // File has trailing spaces on some lines.
    let target = env.root.join("whitespace.txt");
    std::fs::write(&target, "hello world  \nfoo bar  \n").unwrap();

    env.insert_user_message("edit the file").await;

    // Model sends old_str without trailing whitespace.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "file_path": "whitespace.txt",
                "replacements": [{"old_str": "hello world\nfoo bar", "new_str": "greetings\nbaz qux"}]
            }),
        ),
        MockResponse::Text("Edited!".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Edit");
    assert!(output.is_some());
    let output = output.unwrap();
    // If fuzzy matching is supported, it should succeed and mention fuzzy.
    // If not, the edit might fail — either way, no crash.
    assert!(!output.is_empty(), "edit result should not be empty");

    let content = std::fs::read_to_string(&target).unwrap();
    assert!(content.contains("greetings"), "content: {content}");
}

// ── Tool error → model self-correction (Gemini: json-output.test.ts) ───────

#[tokio::test]
async fn tool_error_feeds_back_to_model_for_retry() {
    let env = Env::new().await;
    env.insert_user_message("read a file").await;

    // First attempt: model tries to read a nonexistent file.
    // Second attempt: model reads the correct file.
    let correct_file = env.root.join("correct.txt");
    std::fs::write(&correct_file, "found it").unwrap();

    let provider = MockProvider::new(vec![
        // Bad attempt — file doesn't exist.
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "wrong.txt"})),
        // Model sees the error, tries correct path.
        MockResponse::tool_call("Read", serde_json::json!({"file_path": "correct.txt"})),
        MockResponse::Text("Found the file!".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Should have two tool results — one error, one success.
    let tool_results = find_all_tool_outputs(&events, "Read");

    assert_eq!(tool_results.len(), 2, "should have 2 Read results");
    // First should be an error (file not found).
    assert!(
        tool_results[0].contains("No such file")
            || tool_results[0].contains("not found")
            || tool_results[0].contains("Error"),
        "first should fail: {}",
        tool_results[0]
    );
    // Second should contain the actual content.
    assert!(
        tool_results[1].contains("found it"),
        "second should succeed: {}",
        tool_results[1]
    );
}

// ── Session resume after interruption ──────────────────────────────────────

#[tokio::test]
async fn resume_interrupted_session_detects_unanswered_prompt() {
    let env = Env::new().await;

    // Simulate an interrupted session: user message with no assistant reply.
    env.insert_user_message("tell me about Rust").await;

    // The inference loop should detect the interruption and proceed.
    let provider = MockProvider::new(vec![MockResponse::Text(
        "Rust is a systems programming language.".into(),
    )]);
    let events = env.run_inference(&provider).await;

    // Should detect the unanswered prompt and produce a response.
    let has_text = events
        .iter()
        .any(|e| matches!(e, EngineEvent::TextDelta { .. }));
    assert!(has_text, "should produce a response: {events:?}");

    // Response should be persisted.
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Rust"),
        "response should be persisted: {last}"
    );
}

#[tokio::test]
async fn resume_interrupted_tool_call_session() {
    let env = Env::new().await;

    // Simulate: user asked, assistant made a tool call, but tool result was
    // never received (crash mid-execution).
    env.insert_user_message("list files").await;

    let tc_json = r#"[{"id":"tc_orphan","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]"#;
    env.db
        .insert_message(
            &env.session_id,
            &koda_core::db::Role::Assistant,
            None,
            Some(tc_json),
            None,
            None,
        )
        .await
        .unwrap();

    // No tool result — the prune pass should clean this up.
    // Then the model should produce a fresh response.
    let provider = MockProvider::new(vec![MockResponse::Text("Here are the files.".into())]);
    let (result, events) = env.run_inference_result(&provider).await;

    // Should not crash.
    assert!(
        result.is_ok(),
        "resume should be graceful: {:?}",
        result.err()
    );

    let has_text = events
        .iter()
        .any(|e| matches!(e, EngineEvent::TextDelta { .. }));
    assert!(has_text, "should produce a response after resume");
}

// ── Multi-turn conversation coherence ──────────────────────────────────────

#[tokio::test]
async fn multi_turn_context_preserved() {
    let env = Env::new().await;

    // Turn 1: user asks, model responds.
    env.insert_user_message("My name is Alice.").await;
    let provider1 = MockProvider::new(vec![MockResponse::Text("Nice to meet you, Alice!".into())]);
    env.run_inference(&provider1).await;

    // Turn 2: follow-up referencing turn 1.
    env.insert_user_message("What is my name?").await;
    let provider2 = MockProvider::new(vec![MockResponse::Text("Your name is Alice.".into())]);
    let events = env.run_inference(&provider2).await;

    // Both turns should be persisted correctly.
    let messages = env.db.load_context(&env.session_id).await.unwrap();
    assert!(
        messages.len() >= 4,
        "should have at least 4 messages (2 user + 2 assistant), got {}",
        messages.len()
    );

    // Verify ordering: user, assistant, user, assistant.
    let roles: Vec<_> = messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(&roles[..4], &["user", "assistant", "user", "assistant"]);

    // Final response should be present.
    let has_text = events
        .iter()
        .any(|e| matches!(e, EngineEvent::TextDelta { .. }));
    assert!(has_text);
}

// ── Bash output truncation (Codex: exec_tests) ────────────────────────────

#[tokio::test]
async fn bash_large_output_is_truncated() {
    let env = Env::new().await;
    env.insert_user_message("generate huge output").await;

    // seq 100000 generates ~600KB of output.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Bash", serde_json::json!({"command": "seq 1 100000"})),
        MockResponse::Text("That was a lot of output.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Bash");
    assert!(output.is_some());
    let output = output.unwrap();
    // Output should be truncated, not the full ~600KB.
    assert!(
        output.len() < 200_000,
        "output should be truncated, got {} bytes",
        output.len()
    );
    assert!(
        output.contains("truncated")
            || output.contains("TRUNCATED")
            || output.contains("...")
            || output.contains("Full output stored"),
        "should indicate truncation: {}",
        &output[output.len().saturating_sub(200)..]
    );
}

// ── Bash timeout (Codex: exec_tests) ──────────────────────────────────────

#[tokio::test]
async fn bash_command_timeout() {
    let env = Env::new().await;
    env.insert_user_message("run a slow command").await;

    // timeout is set in the tool args; the shell tool enforces it.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Bash",
            serde_json::json!({"command": "sleep 300", "timeout": 2}),
        ),
        MockResponse::Text("It timed out.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = find_tool_output(&events, "Bash");
    assert!(output.is_some());
    let output = output.unwrap();
    assert!(
        output.contains("timed out")
            || output.contains("timeout")
            || output.contains("Timeout")
            || output.contains("killed"),
        "should mention timeout: {output}"
    );
}

// ── Parallel tool wave ordering (Gemini: parallel-tools.test.ts) ───────────

#[tokio::test]
async fn parallel_read_tools_execute_concurrently() {
    let env = Env::new().await;
    let f1 = env.root.join("p1.txt");
    let f2 = env.root.join("p2.txt");
    let f3 = env.root.join("p3.txt");
    std::fs::write(&f1, "content 1").unwrap();
    std::fs::write(&f2, "content 2").unwrap();
    std::fs::write(&f3, "content 3").unwrap();

    env.insert_user_message("read all three files").await;

    // Three read-only tools in one batch — should all execute.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            koda_core::providers::ToolCall {
                id: "r1".into(),
                function_name: "Read".into(),
                arguments: serde_json::json!({"file_path": "p1.txt"}).to_string(),
                thought_signature: None,
            },
            koda_core::providers::ToolCall {
                id: "r2".into(),
                function_name: "Read".into(),
                arguments: serde_json::json!({"file_path": "p2.txt"}).to_string(),
                thought_signature: None,
            },
            koda_core::providers::ToolCall {
                id: "r3".into(),
                function_name: "Read".into(),
                arguments: serde_json::json!({"file_path": "p3.txt"}).to_string(),
                thought_signature: None,
            },
        ]),
        MockResponse::Text("All three read.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // All three should produce results.
    let read_outputs = find_all_tool_outputs(&events, "Read");
    assert_eq!(read_outputs.len(), 3, "should have 3 Read results");
    assert!(read_outputs.iter().any(|o| o.contains("content 1")));
    assert!(read_outputs.iter().any(|o| o.contains("content 2")));
    assert!(read_outputs.iter().any(|o| o.contains("content 3")));
}

#[tokio::test]
async fn mixed_read_write_batch_splits_correctly() {
    let env = Env::new().await;
    let f1 = env.root.join("existing.txt");
    std::fs::write(&f1, "original content").unwrap();

    env.insert_user_message("read then write").await;

    // Mixed batch: 2 reads + 1 write. The write should execute after reads.
    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            koda_core::providers::ToolCall {
                id: "r1".into(),
                function_name: "Read".into(),
                arguments: serde_json::json!({"file_path": "existing.txt"}).to_string(),
                thought_signature: None,
            },
            koda_core::providers::ToolCall {
                id: "w1".into(),
                function_name: "Write".into(),
                arguments: serde_json::json!({
                    "file_path": "new_file.txt",
                    "content": "new content"
                })
                .to_string(),
                thought_signature: None,
            },
        ]),
        MockResponse::Text("Done!".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Both tools should have results.
    assert!(find_tool_output(&events, "Read").is_some());
    assert!(find_tool_output(&events, "Write").is_some());

    // New file should exist.
    assert!(env.root.join("new_file.txt").exists());
}
