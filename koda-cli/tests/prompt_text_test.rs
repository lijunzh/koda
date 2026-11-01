//! A real headless run: the model gets the prompt exactly as it was typed.
//!
//! Code, diffs and Markdown depend on their newlines and indentation, so this
//! checks the request that reaches the provider, not the input module alone.

use koda_test_utils::network::FakeLlmServer;
use serde_json::{Value, json};
use std::time::Duration;

const PROMPT: &str = "Repair:\n```python\ndef f():\n    return 1\n```\n\n- keep this\n";

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multiline_prompt_reaches_the_model_unchanged() {
    let server = FakeLlmServer::spawn().await;
    let chunks = [
        json!({"choices": [{"delta": {"content": "Done."}}]}).to_string(),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string(),
        "[DONE]".to_string(),
    ];
    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
    server
        .mount_sse("POST", r".*/chat/completions$", &chunks)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(koda_bin())
        .args(["-p", PROMPT, "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .args(["--max-context-tokens", "100000"])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(60), child)
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let received = server.received_requests().await;
    let request = received
        .iter()
        .find(|r| r.url.path().ends_with("/chat/completions"))
        .expect("no chat request");
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|m| m["role"] == "user")
        .expect("no user message");
    assert_eq!(user["content"], PROMPT, "{user}");
}
