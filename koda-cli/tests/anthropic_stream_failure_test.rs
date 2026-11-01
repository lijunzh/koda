//! A real headless run on the Anthropic provider when the stream fails.
//!
//! The stream has already returned HTTP 200 by then, so the failure can only
//! arrive as an `error` event or as a stream that stops short. Either way the
//! partial reply must not count as finished, and its tool calls must not run.

use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Output;
use std::time::Duration;

const TARGET: &str = "written.txt";

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// A reply that says something, then finishes a `Write` call.
fn partial_reply() -> Vec<String> {
    let input = json!({"file_path": TARGET, "content": "should not exist"});
    [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial answer"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Write", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
        json!({"type": "content_block_stop", "index": 1}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

async fn run(chunks: &[String], project: &Path) -> Output {
    let server = FakeLlmServer::spawn().await;
    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
    server.mount_sse("POST", r".*/v1/messages$", &chunks).await;

    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Write the file", "--output-format", "json"])
        .args(["--provider", "anthropic", "--model", "claude-haiku-5-5"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
        .args(["--mode", "auto"])
        .arg("--project-root")
        .arg(project)
        .env("XDG_CONFIG_HOME", project)
        .env("ANTHROPIC_API_KEY", "test-key")
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(Duration::from_secs(60), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda")
}

fn assert_discarded(output: &Output, project: &Path, reason: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !project.join(TARGET).exists(),
        "the tool call ran: {stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(reason) || stderr.contains(reason),
        "no mention of {reason}: {stdout}\n{stderr}"
    );
    let start = stdout.find('{').expect("no JSON on stdout");
    let result: Value = serde_json::from_str(&stdout[start..]).unwrap();
    assert_ne!(
        result["response"], "partial answer",
        "the partial reply counted as finished: {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_event_discards_the_reply_and_its_tool_calls() {
    let mut chunks = partial_reply();
    chunks.push(
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
            .to_string(),
    );

    let tmp = tempfile::tempdir().unwrap();
    let output = run(&chunks, tmp.path()).await;
    assert_discarded(&output, tmp.path(), "overloaded_error");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_with_no_message_stop_discards_the_reply_and_its_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run(&partial_reply(), tmp.path()).await;
    assert_discarded(&output, tmp.path(), "message_stop");
}
