//! The `model call` line keeps the stop reason, wherever a stream puts usage.
//!
//! OpenAI's `include_usage` sends usage after the finish chunk, which used to
//! clear the reason. This runs the `koda` binary against a local stub in both
//! shapes. No API key, no model.

use koda_core::loop_guard::MAX_TRUNCATED_REPLIES;
use koda_test_utils::log::{log_line, read_log};
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::time::Duration;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// Run one prompt against a stub that answers with `chunks`; return the log.
async fn run_koda(chunks: Vec<Value>) -> String {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| sse(&chunks))
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Say done", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_log(tmp.path())
}

fn text() -> Value {
    json!({"choices": [{"delta": {"content": "Done."}}]})
}

fn usage() -> Value {
    json!({"prompt_tokens": 11, "completion_tokens": 2})
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_after_the_finish_chunk_keeps_the_stop_reason() {
    let log = run_koda(vec![
        text(),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": usage()}),
    ])
    .await;

    log_line(
        &log,
        "model call",
        &["stop_reason=stop", "failed_stop=false", "prompt_tokens=11"],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_in_the_finish_chunk_keeps_the_stop_reason() {
    let log = run_koda(vec![
        text(),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": usage()}),
    ])
    .await;

    log_line(
        &log,
        "model call",
        &["stop_reason=stop", "failed_stop=false", "prompt_tokens=11"],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_length_finish_before_usage_logs_max_tokens() {
    let log = run_koda(vec![
        text(),
        json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
        json!({"choices": [], "usage": usage()}),
    ])
    .await;

    log_line(
        &log,
        "model call",
        &["stop_reason=max_tokens", "prompt_tokens=11"],
    );
    // The stub cuts off every reply, and the run still has to end.
    let calls = log.lines().filter(|l| l.contains("model call")).count();
    assert_eq!(calls as u32, MAX_TRUNCATED_REPLIES, "{log}");
}
