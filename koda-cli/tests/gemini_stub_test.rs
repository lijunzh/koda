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

/// Everything koda logged for a run whose config dir was `config`.
fn koda_log(config: &std::path::Path) -> String {
    std::fs::read_dir(config.join("koda/logs"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("koda-"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

fn sse(chunk: Value) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {chunk}\n\n"))
}

fn read_call(file_path: &str) -> ResponseTemplate {
    sse(json!({"candidates": [{
        "content": {"parts": [{"functionCall": {
            "name": "Read",
            "args": {"file_path": file_path},
        }}]},
        "finishReason": "STOP",
    }]}))
}

fn reply(request: usize) -> ResponseTemplate {
    match request {
        0 => ResponseTemplate::new(503).set_body_string(
            r#"{"error":{"code":503,"message":"This model is currently experiencing high demand."}}"#,
        ),
        1 => read_call("notes.txt"),
        2 => read_call("more.txt"),
        3 => sse(json!({"candidates": [{"finishReason": "MALFORMED_FUNCTION_CALL"}]})),
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
    std::fs::write(tmp.path().join("notes.txt"), "first file\n").unwrap();
    std::fs::write(tmp.path().join("more.txt"), "second file\n").unwrap();
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
    let log = koda_log(tmp.path());
    assert!(log.contains("Rate limited"), "the 503: {log}");
    assert!(
        log.contains("stop_reason=malformed_function_call failed_stop=true"),
        "{log}"
    );
    assert!(log.contains(r#"provider="gemini""#), "{log}");

    assert_eq!(turns.len(), 5, "the 503 and the failed reply are retried");

    let last = &turns[4];
    assert!(last["generationConfig"].get("maxOutputTokens").is_none());
    let contents = last["contents"].as_array().unwrap();
    let call = &contents[contents.len() - 2];
    assert_eq!(call["role"], "model");
    assert!(call["parts"][0]["thoughtSignature"].is_string(), "{call}");
    let result_turn = &contents[contents.len() - 1];
    assert_eq!(result_turn["role"], "user");
    let response = &result_turn["parts"][0]["functionResponse"];
    assert_eq!(response["name"], "Read", "{result_turn}");

    // Every result stays in history, not only the newest (#1454).
    let sent = last["contents"].to_string();
    assert!(sent.contains("first file"), "{sent}");
    assert!(sent.contains("second file"), "{sent}");
}

/// #1449: a mid-history `system` message used to replace Gemini's system
/// prompt. After a compaction the prompt must be the same and the summary must
/// arrive in `contents`, which only ever holds `user` and `model` turns.
#[tokio::test(flavor = "multi_thread")]
async fn compaction_leaves_the_system_prompt_alone() {
    const FILES: usize = 40;
    const SUMMARY: &str = "The user asked to read the notes.";

    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*:generateContent$", |_| {
            ResponseTemplate::new(200).set_body_json(json!({"candidates": [{
                "content": {"parts": [{"text": format!("<summary>{SUMMARY}</summary>")}]},
                "finishReason": "STOP",
            }]}))
        })
        .await;
    let turn = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*:streamGenerateContent$", move |request| {
            let body = String::from_utf8_lossy(&request.body);
            let turn = turn.fetch_add(1, Ordering::SeqCst);
            if turn >= FILES || body.contains(SUMMARY) {
                reply(usize::MAX)
            } else {
                read_call(&format!("notes_{turn}.txt"))
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    for i in 0..FILES {
        let text = format!("note {i}: nothing to see in this file\n").repeat(400);
        std::fs::write(tmp.path().join(format!("notes_{i}.txt")), text).unwrap();
    }
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", "Read the notes", "--output-format", "json"])
        .args(["--provider", "gemini", "--model", "gemini-flash-latest"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "12000"])
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
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let received = server.received_requests().await;
    let turns: Vec<Value> = received
        .iter()
        .filter(|r| r.url.path().contains("streamGenerateContent"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let (first, last) = (&turns[0], &turns[turns.len() - 1]);
    assert!(last.to_string().contains(SUMMARY), "never compacted");
    let log = koda_log(tmp.path());
    assert!(log.contains(r#"purpose="compaction""#));
    assert!(log.contains("context assembled"), "{log}");
    assert!(log.contains("tool_calls_chars="), "{log}");
    assert!(log.contains("rows_summarized="), "{log}");
    assert!(log.contains("turn end"), "{log}");
    assert!(first["systemInstruction"]["parts"][0]["text"].is_string());
    assert_eq!(first["systemInstruction"], last["systemInstruction"]);
    for content in last["contents"].as_array().unwrap() {
        assert!(
            content["role"] == "user" || content["role"] == "model",
            "{content}"
        );
    }
}
