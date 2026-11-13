//! A real headless run on an OpenAI-compatible server when the stream fails.
//!
//! The stream has already returned HTTP 200 by then, so the failure can only
//! arrive as an error object or as a stream that stops short. Either way the
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

/// A reply that says something, then starts a `Write` call, and doesn't finish.
fn partial_reply() -> Vec<String> {
    let input = json!({"file_path": TARGET, "content": "should not exist"});
    let call = json!({"index": 0, "id": "c1", "function": {"name": "Write", "arguments": input.to_string()}});
    [
        json!({"choices": [{"delta": {"content": "partial answer"}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]}),
    ]
    .iter()
    .map(Value::to_string)
    .collect()
}

async fn run(chunks: &[String], project: &Path) -> Output {
    let server = FakeLlmServer::spawn().await;
    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
    server
        .mount_sse("POST", r".*/chat/completions$", &chunks)
        .await;

    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Write the file", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
        .args(["--mode", "auto"])
        .arg("--project-root")
        .arg(project)
        .env("XDG_CONFIG_HOME", project)
        // What's checked is what a failed stream leaves behind; retrying it
        // only adds the backoff's 31 seconds.
        .env("KODA_REQUEST_MAX_RETRIES", "0")
        .env("KODA_STREAM_MAX_RETRIES", "0")
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
async fn an_error_object_discards_the_reply_and_its_tool_calls() {
    let mut chunks = partial_reply();
    chunks.push(json!({"error": {"message": "worker failed", "type": "server_error"}}).to_string());

    let tmp = tempfile::tempdir().unwrap();
    let output = run(&chunks, tmp.path()).await;
    assert_discarded(&output, tmp.path(), "worker failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_that_stops_short_discards_the_reply_and_its_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let output = run(&partial_reply(), tmp.path()).await;
    assert_discarded(&output, tmp.path(), "finish_reason");
}
