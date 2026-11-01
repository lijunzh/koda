//! SIGINT during a model call, in a real headless run.
//!
//! The eval harness stops a run that hits its time limit this way, so what
//! koda reports here is all that survives of that run: the turn end, the run
//! summary and the log. No API key, no model.
#![cfg(unix)]

use koda_core::inference::CANCEL_GRACE;
use koda_test_utils::log::read_log;
use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn sigint_during_a_model_call_still_reports_the_turn() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |_| {
            ResponseTemplate::new(200).set_delay(Duration::from_secs(120))
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Fix the parser", "--output-format", "stream-json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Signal once the chat request is in flight. Earlier requests are startup
    // probes, sent before koda listens for SIGINT, so the signal would kill it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while chat_requests(&server).await == 0 {
        assert!(tokio::time::Instant::now() < deadline, "no model call");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = child.id().unwrap().to_string();
    let sent = std::process::Command::new("kill")
        .args(["-s", "INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let of_type = |kind: &str| events.iter().find(|e| e["type"] == kind);
    let turn_end = of_type("turn_end").unwrap_or_else(|| panic!("no turn_end: {stdout}"));
    assert_eq!(turn_end["reason"]["kind"], "cancelled", "{stdout}");
    let summary = of_type("run_summary").unwrap_or_else(|| panic!("no summary: {stdout}"));
    assert_eq!(summary["turn_end"]["kind"], "cancelled", "{stdout}");

    let log = read_log(tmp.path());
    assert!(log.contains(r#"outcome="cancelled""#), "{log}");
}

/// The parent delegates, and the sub-agent's first call returns a Read. Its
/// second call never answers, and nothing in the child listens for the cancel
/// while it waits.
async fn stub_with_a_stuck_sub_agent() -> FakeLlmServer {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", |request| {
            let request: Value = serde_json::from_slice(&request.body).unwrap();
            if request["stream"] == json!(true) {
                return delegate();
            }
            let has_result = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "tool");
            if has_result {
                return ResponseTemplate::new(200).set_delay(Duration::from_secs(120));
            }
            child_reads_a_file()
        })
        .await;
    server
}

fn delegate() -> ResponseTemplate {
    let call = json!({
        "index": 0,
        "id": "agent",
        "type": "function",
        "function": {
            "name": "InvokeAgent",
            "arguments": json!({"agent_name": "explore", "prompt": "look"}).to_string(),
        },
    });
    let chunks = [
        json!({"choices": [{"delta": {"tool_calls": [call]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ];
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn child_reads_a_file() -> ResponseTemplate {
    let call = json!({
        "id": "read",
        "type": "function",
        "function": {"name": "Read", "arguments": json!({"file_path": "a.txt"}).to_string()},
    });
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{
            "message": {"role": "assistant", "tool_calls": [call]},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 30, "completion_tokens": 3},
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_agent_that_ignores_sigint_still_closes_the_turn() {
    let server = stub_with_a_stuck_sub_agent().await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Fix the parser", "--output-format", "stream-json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    // Parent, child's Read, then the child call that hangs.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while chat_requests(&server).await < 3 {
        assert!(tokio::time::Instant::now() < deadline, "child never hung");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = child.id().unwrap().to_string();
    let signalled = Instant::now();
    let sent = std::process::Command::new("kill")
        .args(["-s", "INT", &pid])
        .status()
        .unwrap();
    assert!(sent.success());

    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("koda ignored SIGINT")
        .unwrap();
    assert!(
        signalled.elapsed() >= CANCEL_GRACE,
        "the turn had time to stop"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let of_type =
        |kind: &str| -> Vec<&Value> { events.iter().filter(|e| e["type"] == kind).collect() };

    let ends = of_type("turn_end");
    assert_eq!(ends.len(), 1, "closed once: {stdout}");
    assert_eq!(ends[0]["reason"]["kind"], "cancelled", "{stdout}");

    // What finished before the signal still counts: the parent and one child call.
    let summaries = of_type("turn_summary");
    assert_eq!(summaries.len(), 1, "{stdout}");
    let usage = &summaries[0]["usage"];
    assert_eq!(usage["api_calls"], 2, "{usage}");
    assert_eq!(usage["prompt_tokens"], 130, "{usage}");

    let last = events.last().unwrap();
    assert_eq!(last["type"], "run_summary", "{stdout}");
    assert_eq!(last["success"], false, "{stdout}");
    assert_eq!(last["turn_end"]["kind"], "cancelled", "{stdout}");
    assert!(last["error"].is_string(), "{stdout}");
    assert_eq!(output.status.code(), Some(1), "{stdout}");

    let log = read_log(tmp.path());
    assert!(log.contains(r#"outcome="abandoned""#), "{log}");
}

async fn chat_requests(server: &FakeLlmServer) -> usize {
    server
        .received_requests()
        .await
        .iter()
        .filter(|request| request.url.path().ends_with("/chat/completions"))
        .count()
}
