//! The provider's token count drives compaction, in a real headless run.
//!
//! The estimate runs low, so a run could pass its window while it read 70%
//! (#1454). This runs the `koda` binary against a local stub that reports a
//! prompt far above the estimate, or nothing at all. No API key, no model.

use koda_test_utils::log::{log_line, read_log};
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const WINDOW: &str = "40000";
const REPORTED: i64 = 37_000;
const READS: usize = 4;

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

fn read_call(index: usize, usage: &Value) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": format!("call_{index}"),
            "type": "function",
            "function": {
                "name": "Read",
                "arguments": json!({"file_path": format!("notes_{index}.txt")}).to_string(),
            },
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": usage}),
    ])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": "Done."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// Run a turn that reads [`READS`] files, with `usage` on every reply.
async fn run_koda(usage: Value) -> String {
    let server = FakeLlmServer::spawn().await;
    let turns = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] != json!(true) {
                return ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "<summary>Read notes.</summary>"},
                        "finish_reason": "stop",
                    }],
                }));
            }
            let turn = turns.fetch_add(1, Ordering::SeqCst);
            if turn < READS {
                read_call(turn, &usage)
            } else {
                final_text()
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    for i in 0..READS {
        let line = format!("note {i}: nothing to see in this file\n");
        std::fs::write(tmp.path().join(format!("notes_{i}.txt")), line.repeat(400)).unwrap();
    }
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Read the notes", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", WINDOW])
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

#[tokio::test(flavor = "multi_thread")]
async fn a_reported_count_above_the_estimate_triggers_compaction() {
    let log = run_koda(json!({"prompt_tokens": REPORTED, "completion_tokens": 5})).await;

    log_line(
        &log,
        "model call",
        &[r#"purpose="turn""#, "context_tokens=37000"],
    );
    log_line(&log, "context assembled", &["correction="]);
    let compaction = log_line(&log, "compaction", &[r#"branch="summary""#]);

    // The count described the old context, so it must not outlive the compaction.
    let (_, after) = log.split_at(log.find(compaction).unwrap());
    log_line(after, "context assembled", &["correction=0"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_count_the_estimate_decides() {
    let log = run_koda(json!({})).await;

    log_line(
        &log,
        "model call",
        &[r#"purpose="turn""#, "context_tokens=0"],
    );
    log_line(&log, "context assembled", &["correction=0"]);
    assert!(!log.contains("branch="), "no compaction expected: {log}");
}
