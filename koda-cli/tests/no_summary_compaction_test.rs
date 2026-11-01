//! Compaction with no summary, in a real headless run.
//!
//! The no-summary path runs only when a summary fails, so eval runs never
//! reach it and the unit tests in `compact_reasoning_test.rs` stop at the
//! database. These run the `koda` binary against a local OpenAI-compatible
//! stub (#1455): streaming requests are the agent's own turns, which read
//! files until the context fills, and non-streaming requests are summary
//! calls, which the stub empties or fails. No API key, no model.

use koda_test_utils::network::{FakeLlmServer, Request, ResponseTemplate};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TASK: &str = "Fix the parser in lexer.rs";
const MARKER: &str = "(no summary available)";
const FINAL_TEXT: &str = "Finished the parser fix.";
const FILES: usize = 40;
const WINDOW: &str = "12000";

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

fn read_file_call(index: usize) -> ResponseTemplate {
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
    ])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": FINAL_TEXT}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

/// All the message text in a request.
fn text_of(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stub an agent that reads files until the marker shows up in its context,
/// then finishes. Summary calls get `summary_reply`.
async fn spawn_stub(
    summary_reply: impl Fn() -> ResponseTemplate + Send + Sync + 'static,
) -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    let turns = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |request| {
            let request = body(request);
            if request["stream"] != json!(true) {
                return summary_reply();
            }
            let turn = turns.fetch_add(1, Ordering::SeqCst);
            if turn >= FILES || text_of(&request).contains(MARKER) {
                final_text()
            } else {
                read_file_call(turn)
            }
        })
        .await;
    server
}

fn seed_project(root: &Path) {
    for i in 0..FILES {
        let line = format!("note {i}: nothing to see in this file\n");
        std::fs::write(root.join(format!("notes_{i}.txt")), line.repeat(400)).unwrap();
    }
}

/// Everything koda logged for a run whose config dir was `config`.
fn koda_log(config: &Path) -> String {
    std::fs::read_dir(config.join("koda/logs"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("koda-"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

/// Run a headless turn against `server`, returning stdout's JSON, the
/// requests the server saw, in order, and koda's log.
async fn run_koda(server: &FakeLlmServer) -> (Value, Vec<Value>, String) {
    let tmp = tempfile::tempdir().unwrap();
    seed_project(tmp.path());
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", TASK, "--output-format", "json"])
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let start = stdout.find('{').expect("no JSON on stdout");
    let result: Value = serde_json::from_str(&stdout[start..]).unwrap();
    // Skip koda's model-listing GETs.
    let requests = server.received_requests().await;
    let chats = requests.iter().filter(|r| !r.body.is_empty());
    (result, chats.map(body).collect(), koda_log(tmp.path()))
}

/// The run went through a no-summary compaction and carried on: the agent's
/// first request after it has the marker and the task, and the turn finished.
fn assert_task_survived(result: &Value, requests: &[Value]) {
    assert_eq!(result["success"], true, "{result}");
    assert_eq!(result["response"], FINAL_TEXT, "{result}");

    let summary_calls = requests.iter().filter(|r| r["stream"] != json!(true));
    assert!(
        summary_calls.count() > 0,
        "the run never tried to summarize"
    );

    let turns: Vec<&Value> = requests
        .iter()
        .filter(|r| r["stream"] == json!(true))
        .collect();
    assert!(!text_of(turns[0]).contains(MARKER));
    let after = turns
        .iter()
        .position(|r| text_of(r).contains(MARKER))
        .expect("no request carried the marker");
    assert_eq!(after, turns.len() - 1, "the stub finishes on the marker");

    // The user's message itself was replaced, so the marker is what carries it.
    let carriers: Vec<&str> = turns[after]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .filter(|text| text.contains(TASK))
        .collect();
    assert_eq!(
        carriers.len(),
        1,
        "the task must reach the next request once"
    );
    assert!(carriers[0].contains(MARKER), "{}", carriers[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_summary_keeps_the_task_and_the_run_carries_on() {
    let server = spawn_stub(|| {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": ""},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 0},
        }))
    })
    .await;

    let (result, requests, log) = run_koda(&server).await;

    assert_task_survived(&result, &requests);
    assert!(log.contains(r#"branch="empty""#), "{log}");
    assert!(log.contains(r#"outcome="complete""#), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_summary_keeps_the_task_and_the_run_carries_on() {
    // Every summary call fails, so nothing shrinks the history until it
    // passes the window and the pre-flight check drops it with no model call.
    let server = spawn_stub(|| ResponseTemplate::new(500).set_body_string("summary failed")).await;

    let (result, requests, log) = run_koda(&server).await;

    assert_task_survived(&result, &requests);
    assert!(log.contains(r#"branch="dropped""#), "{log}");
}
