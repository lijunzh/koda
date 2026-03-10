//! Smoke tests: headless mode with MockProvider.
//!
//! These tests exercise the full binary pipeline without a real LLM.
//! They run `koda -p "..." --provider mock --output-format json`
//! with scripted responses via KODA_MOCK_RESPONSES env var.
//!
//! CI-safe: no network, no API keys, no LLM required.

use std::process::Command;

// ── Helpers ─────────────────────────────────────────────────

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn run_mock(prompt: &str, responses: &str) -> (String, String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(koda_bin())
        .args([
            "-p",
            prompt,
            "--provider",
            "mock",
            "--skip-probe",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses)
        .output()
        .expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

fn extract_json(stdout: &str) -> serde_json::Value {
    // The JSON is pretty-printed across multiple lines.
    // Find the opening '{' and collect everything from there.
    let start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("No JSON object in stdout:\n{stdout}"));
    serde_json::from_str(&stdout[start..])
        .unwrap_or_else(|e| panic!("Invalid JSON: {e}\nfrom: {}", &stdout[start..]))
}
// ── Headless MockProvider tests ──────────────────────────────

#[test]
fn mock_text_response_returns_json() {
    let (stdout, stderr, success) = run_mock("say hi", r#"[{"text":"Hello from mock!"}]"#);
    assert!(success, "Process failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("Hello from mock"),
        "Expected 'Hello from mock' in response, got: {response}"
    );
}

#[test]
fn mock_empty_responses_succeeds() {
    let (stdout, stderr, success) = run_mock("say hi", "[]");
    assert!(success, "Process failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
}

#[test]
fn mock_tool_use_read_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "mock test content").unwrap();

    let responses = r#"[
        {"tool":"Read","args":{"path":"hello.txt"}},
        {"text":"I read the file."}
    ]"#;

    let output = Command::new(koda_bin())
        .args([
            "-p",
            "read hello.txt",
            "--provider",
            "mock",
            "--skip-probe",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses)
        .output()
        .expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "Failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("read the file"),
        "Expected tool result in response, got: {response}\nstderr: {stderr}"
    );
}

#[test]
fn mock_error_response_handled() {
    let (stdout, _stderr, _success) = run_mock("say hi", r#"[{"error":"Simulated LLM failure"}]"#);
    let json = extract_json(&stdout);
    // Provider error → success=false or empty response
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        json["success"] == false || response.is_empty(),
        "Expected failure indication in: {json}"
    );
}

#[test]
fn mock_session_id_returned() {
    let (stdout, stderr, _) = run_mock("say hi", r#"[{"text":"ok"}]"#);
    let json = extract_json(&stdout);
    let session_id = json["session_id"].as_str();
    assert!(
        session_id.is_some() && !session_id.unwrap().is_empty(),
        "Expected session_id in JSON.\nJSON: {json}\nstderr: {stderr}"
    );
}

#[test]
fn mock_model_name_in_json() {
    let (stdout, _, _) = run_mock("say hi", r#"[{"text":"ok"}]"#);
    let json = extract_json(&stdout);
    let model = json["model"].as_str().unwrap_or("");
    // Mock provider reports "mock-model" but config may load "auto-detect" first
    assert!(!model.is_empty(), "Expected model name in JSON, got empty");
}

#[test]
fn mock_at_file_reference() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("data.txt"), "important data").unwrap();

    let output = Command::new(koda_bin())
        .args([
            "-p",
            "analyze @data.txt",
            "--provider",
            "mock",
            "--skip-probe",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"analyzed"}]"#)
        .output()
        .expect("Failed to run koda");

    assert!(
        output.status.success(),
        "@file processing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mock_multi_turn_tool_use() {
    let responses = r#"[
        {"tool":"Bash","args":{"command":"echo hello"}},
        {"text":"Command output was hello."}
    ]"#;
    let (stdout, stderr, success) = run_mock("run echo hello", responses);
    assert!(success, "Multi-turn failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
}

#[test]
fn mock_session_resume() {
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1
    let output1 = Command::new(koda_bin())
        .args([
            "-p",
            "turn one",
            "--provider",
            "mock",
            "--skip-probe",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"first"}]"#)
        .output()
        .expect("Turn 1 failed");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let json1 = extract_json(&stdout1);
    let session_id = json1["session_id"].as_str().expect("No session_id");

    // Turn 2: resume
    let output2 = Command::new(koda_bin())
        .args([
            "-p",
            "turn two",
            "--provider",
            "mock",
            "--skip-probe",
            "--output-format",
            "json",
            "--session",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"second"}]"#)
        .output()
        .expect("Turn 2 failed");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(
        !stdout2.is_empty(),
        "Turn 2 produced no stdout.\nstderr: {stderr2}"
    );
    let json2 = extract_json(&stdout2);
    assert_eq!(json2["success"], true);
    assert_eq!(
        json2["session_id"].as_str().unwrap(),
        session_id,
        "Resumed session should keep same ID"
    );
}
