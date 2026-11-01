//! A tool result hidden from the model must show in the log.
//!
//! `load_context` drops rows the model never sees, and nothing else records
//! that. Repeated call ids hid all but the newest result from Gemini for a
//! whole run, unseen until a log line was added (#1454). This runs the `koda`
//! binary against a local stub that repeats an id.

use koda_test_utils::log::{log_line, read_log};
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FINAL_TEXT: &str = "All done.";

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

fn read_call(file_path: &str) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_repeated",
            "type": "function",
            "function": {
                "name": "Read",
                "arguments": json!({"file_path": file_path}).to_string(),
            },
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": FINAL_TEXT}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_tool_result_is_logged_as_a_warning() {
    let server = FakeLlmServer::spawn().await;
    let turn = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            match turn.fetch_add(1, Ordering::SeqCst) {
                0 => read_call("first.txt"),
                1 => read_call("second.txt"),
                _ => final_text(),
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("first.txt"), "first file\n").unwrap();
    std::fs::write(tmp.path().join("second.txt"), "second file\n").unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Read the files", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
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

    let log = read_log(tmp.path());
    let line = log_line(
        &log,
        "history edited",
        &[
            r#"pass="dedupe_tool_results_by_call_id""#,
            "rows=1",
            "tool_rows=1",
            "chars=",
            "ids=",
        ],
    );
    assert!(line.contains("WARN"), "a lost result is a warning: {line}");
}
