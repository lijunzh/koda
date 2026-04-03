//! E2E tests: tool coverage (Glob, Grep, Edit, Delete, AST verification).
//!
//! Covers tools that are only unit-tested elsewhere, plus v0.1.14
//! regression tests for post-edit AST verification (#467/#504).

mod e2e_harness;

use e2e_harness::Env;
use koda_core::{
    engine::EngineEvent,
    providers::mock::{MockProvider, MockResponse},
};

// ── Glob ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_glob_tool_in_sandbox() {
    let env = Env::new().await;

    let src_dir = env.root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub mod foo;").unwrap();

    env.insert_user_message("find rust files").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("Glob", serde_json::json!({"pattern": "src/*.rs"})),
        MockResponse::Text("Found 2 Rust files.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Glob"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Glob tool result");
    assert!(output.contains("main.rs"));
    assert!(output.contains("lib.rs"));
}

// ── Grep ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_grep_tool_in_sandbox() {
    let env = Env::new().await;

    std::fs::write(
        env.root.join("hello.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();
    std::fs::write(
        env.root.join("bye.rs"),
        "fn goodbye() { println!(\"bye\"); }",
    )
    .unwrap();

    env.insert_user_message("search for println").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Grep",
            serde_json::json!({"pattern": "println", "path": "."}),
        ),
        MockResponse::Text("Found println in two files.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Grep"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Grep tool result");
    assert!(
        output.contains("hello.rs"),
        "should find match in hello.rs: {output}"
    );
    assert!(
        output.contains("bye.rs"),
        "should find match in bye.rs: {output}"
    );
}

#[tokio::test]
async fn test_grep_no_matches_in_sandbox() {
    let env = Env::new().await;

    std::fs::write(env.root.join("data.txt"), "no matches here").unwrap();

    env.insert_user_message("search for zzzzz").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Grep",
            serde_json::json!({"pattern": "zzzzz_nonexistent", "path": "."}),
        ),
        MockResponse::Text("Nothing found.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Grep"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Grep tool result");
    // No matches should still return successfully with an indication
    assert!(
        output.to_lowercase().contains("no match")
            || output.to_lowercase().contains("0 match")
            || output.is_empty()
            || output.contains("No results"),
        "should indicate no matches: {output}"
    );
}

// ── Edit (replacements) ────────────────────────────────────────

#[tokio::test]
async fn test_edit_tool_replacement_e2e() {
    let env = Env::new().await;

    let target = env.root.join("config.toml");
    std::fs::write(&target, "port = 8080\nhost = \"localhost\"").unwrap();

    env.insert_user_message("change the port").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "replacements": [
                    {"old_str": "port = 8080", "new_str": "port = 3000"}
                ]
            }),
        ),
        MockResponse::Text("Port updated.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Edit"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Edit tool result");
    assert!(
        output.contains("Edit") || output.contains("edit") || output.contains("✓"),
        "should confirm edit: {output}"
    );

    let content = std::fs::read_to_string(&target).unwrap();
    assert!(content.contains("port = 3000"), "file should be updated");
    assert!(
        content.contains("host = \"localhost\""),
        "untouched lines should remain"
    );
}

// ── Delete (standalone) ────────────────────────────────────────

#[tokio::test]
async fn test_delete_tool_standalone_e2e() {
    let env = Env::new().await;

    // Write first (so Koda owns it), then delete in a separate turn
    let target = env.root.join("temp_notes.md");

    // Turn 1: create
    env.insert_user_message("write a temp file").await;
    let p1 = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "temporary notes"
            }),
        ),
        MockResponse::Text("Created.".into()),
    ]);
    env.run_inference(&p1).await;
    assert!(target.exists());

    // Turn 2: delete
    env.insert_user_message("delete the temp file").await;
    let p2 = MockProvider::new(vec![
        MockResponse::tool_call(
            "Delete",
            serde_json::json!({"path": target.to_string_lossy()}),
        ),
        MockResponse::Text("Deleted.".into()),
    ]);
    let events = env.run_inference(&p2).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Delete")),
        "expected Delete tool result"
    );
    assert!(!target.exists(), "file should be gone");
}

// ── AST post-edit verification (#467/#504) ─────────────────────

/// Regression: Write of valid Rust → tool result should NOT contain
/// syntax error warnings.
#[tokio::test]
async fn test_write_valid_rust_no_ast_warning() {
    let env = Env::new().await;
    let target = env.root.join("good.rs");

    env.insert_user_message("create good.rs").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "fn main() {\n    println!(\"hello\");\n}\n"
            }),
        ),
        MockResponse::Text("Done.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let write_output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Write"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Write tool result");

    assert!(
        !write_output.contains("SYNTAX ERROR") && !write_output.contains("syntax error"),
        "valid Rust should not trigger AST warning: {write_output}"
    );
}

/// Regression: Write of broken Rust should still return the normal Write result.
/// Post-edit AST verification was removed in #544, so no syntax warning is appended.
#[tokio::test]
async fn test_write_broken_rust_has_no_ast_warning() {
    let env = Env::new().await;
    let target = env.root.join("broken.rs");

    env.insert_user_message("create broken.rs").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "fn main() {\n    let x = \n}\n"  // missing expr
            }),
        ),
        MockResponse::Text("Write completed.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let write_output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Write"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Write tool result");

    assert!(
        !write_output.contains("SYNTAX ERROR") && !write_output.contains("syntax error"),
        "broken Rust should not trigger AST warning in tool result: {write_output}"
    );
    assert!(
        write_output.contains("Written") || write_output.contains("Wrote"),
        "should still report successful write output: {write_output}"
    );
}

/// Regression: Edit that introduces a syntax error should still return
/// the normal Edit result without post-edit AST warnings.
#[tokio::test]
async fn test_edit_introduces_syntax_error_has_no_ast_warning() {
    let env = Env::new().await;
    let target = env.root.join("fixme.rs");
    std::fs::write(&target, "fn main() {\n    println!(\"ok\");\n}\n").unwrap();

    env.insert_user_message("break the file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "replacements": [
                    {"old_str": "println!(\"ok\");", "new_str": "let x = "}  // incomplete
                ]
            }),
        ),
        MockResponse::Text("Edit completed.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let edit_output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Edit"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Edit tool result");

    assert!(
        !edit_output.contains("SYNTAX ERROR") && !edit_output.contains("syntax error"),
        "edit introducing syntax error should not be flagged by AST: {edit_output}"
    );
    assert!(
        edit_output.contains("Applied 1 edit(s)"),
        "should still report successful edit output: {edit_output}"
    );
}

/// Non-Rust files (e.g., .csv) should not trigger any AST verification.
#[tokio::test]
async fn test_write_non_parseable_file_no_ast_check() {
    let env = Env::new().await;
    let target = env.root.join("data.csv");

    env.insert_user_message("create data.csv").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "name,age\nAlice,30\n"
            }),
        ),
        MockResponse::Text("CSV created.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let write_output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, name, .. } = e
                && name == "Write"
            {
                return Some(output.clone());
            }
            None
        })
        .expect("expected Write tool result");

    assert!(
        !write_output.contains("SYNTAX ERROR") && !write_output.contains("syntax error"),
        "non-parseable file should not trigger AST: {write_output}"
    );
}

// ── Multi-tool single response ─────────────────────────────────

/// LLMs often return multiple tool calls in a single response.
/// Verify the pipeline handles sequential tool execution correctly.
#[tokio::test]
async fn test_multi_tool_sequential_execution() {
    let env = Env::new().await;

    let file_a = env.root.join("a.txt");
    let file_b = env.root.join("b.txt");

    env.insert_user_message("create two files").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": file_a.to_string_lossy(),
                "content": "file A content"
            }),
        ),
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": file_b.to_string_lossy(),
                "content": "file B content"
            }),
        ),
        MockResponse::Text("Both files created.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // Both writes should have executed
    let write_results: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Write"))
        .collect();
    assert_eq!(
        write_results.len(),
        2,
        "expected 2 Write results, got {}",
        write_results.len()
    );

    assert!(file_a.exists(), "file A should exist");
    assert!(file_b.exists(), "file B should exist");
    assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "file A content");
    assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "file B content");
}

/// Read + Write in a single turn: read a file, then write a modified version.
#[tokio::test]
async fn test_read_then_write_single_turn() {
    let env = Env::new().await;

    let source = env.root.join("original.txt");
    std::fs::write(&source, "original content").unwrap();
    let target = env.root.join("copy.txt");

    env.insert_user_message("copy the file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Read",
            serde_json::json!({"path": source.to_string_lossy()}),
        ),
        MockResponse::tool_call(
            "Write",
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "original content (copy)"
            }),
        ),
        MockResponse::Text("Copied.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::ToolCallResult { name, .. } if name == "Read")),
        "expected Read result"
    );
    assert!(target.exists(), "copy should exist");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "original content (copy)"
    );
}

// ── Pre-flight validation (#612) ─────────────────────────────

/// Edit with old_str not found should fail with a validation error
/// and NOT prompt for approval.
#[tokio::test]
async fn test_edit_validation_old_str_not_found() {
    let env = Env::new().await;
    let file = env.root.join("greet.txt");
    std::fs::write(&file, "hello world").unwrap();

    env.insert_user_message("fix the greeting").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "path": "greet.txt",
                "replacements": [{"old_str": "goodbye world", "new_str": "hi"}]
            }),
        ),
        MockResponse::Text("I see the edit failed.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, .. } = e {
                Some(output.clone())
            } else {
                None
            }
        })
        .expect("expected tool result");

    assert!(
        output.contains("not found"),
        "error should mention 'not found': {output}",
    );
    // File should be unchanged
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
}

/// Edit on a non-existent file should fail validation.
#[tokio::test]
async fn test_edit_validation_file_not_found() {
    let env = Env::new().await;

    env.insert_user_message("edit missing file").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "Edit",
            serde_json::json!({
                "path": "nope.txt",
                "replacements": [{"old_str": "x", "new_str": "y"}]
            }),
        ),
        MockResponse::Text("File doesn't exist.".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let output = events
        .iter()
        .find_map(|e| {
            if let EngineEvent::ToolCallResult { output, .. } = e {
                Some(output.clone())
            } else {
                None
            }
        })
        .expect("expected tool result");

    assert!(
        output.contains("Cannot read"),
        "error should mention file not readable: {output}",
    );
}
