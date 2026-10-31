//! A real headless run on the Gemini provider, against a local stub.
//!
//! Covers what the provider's unit tests can't: a busy server, a tool call, a
//! failed reply and the finish, all in one turn through the `koda` binary. The
//! stub replays what `gemini-flash-latest` did in real runs (#1454).

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

fn sse(chunk: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\n"))
}

fn reply(request: usize) -> ResponseTemplate {
    match request {
        0 => ResponseTemplate::new(503).set_body_string(
            r#"{"error":{"code":503,"message":"This model is currently experiencing high demand."}}"#,
        ),
        1 => sse(json!({"candidates": [{
            "content": {"parts": [{"functionCall": {
                "name": "Read",
                "args": {"file_path": "notes.txt"},
            }}]},
            "finishReason": "STOP",
        }]})),
        2 => sse(json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]})),
        _ => sse(json!({"candidates": [{
            "content": {"parts": [{"text": FINAL_TEXT}]},
            "finishReason": "STOP",
        }]})),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_server_and_a_failed_reply_do_not_end_the_turn() {
    let server = FakeLlmServer::spawn().await;
    let requests = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*:streamGenerateContent$", move |_| {
            reply(requests.fetch_add(1, Ordering::SeqCst))
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("notes.txt"), "some notes\n").unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Read notes.txt", "--output-format", "json"])
        .args(["--provider", "gemini", "--model", "gemini-flash-latest"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("GEMINI_API_KEY", "test-key")
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
    assert_eq!(result["response"], FINAL_TEXT, "{result}");

    let received = server.received_requests().await;
    let turns: Vec<Value> = received
        .iter()
        .filter(|r| r.url.path().contains("streamGenerateContent"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(turns.len(), 4, "the 503 and the failed reply are retried");

    let last = &turns[3];
    assert!(last["generationConfig"].get("maxOutputTokens").is_none());
    let contents = last["contents"].as_array().unwrap();
    let call = &contents[contents.len() - 2];
    assert_eq!(call["role"], "model");
    assert!(call["parts"][0]["thoughtSignature"].is_string(), "{call}");
    let result_turn = &contents[contents.len() - 1];
    assert_eq!(result_turn["role"], "user");
    let response = &result_turn["parts"][0]["functionResponse"];
    assert_eq!(response["name"], "Read", "{result_turn}");
}
