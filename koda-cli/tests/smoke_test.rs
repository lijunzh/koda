//! Live smoke tests against a running LM Studio instance.
//!
//! Gated by `KODA_TEST_LMSTUDIO=1` environment variable.
//! These tests are `#[ignore]` by default — run them with:
//!
//!   KODA_TEST_LMSTUDIO=1 cargo test -p koda-cli --test smoke_test -- --ignored

use std::process::Command;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn lmstudio_available() -> bool {
    std::env::var("KODA_TEST_LMSTUDIO").is_ok()
}

#[test]
#[ignore]
fn test_headless_prompt_returns_response() {
    if !lmstudio_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(koda_bin())
        .args([
            "-p",
            "Reply with only the word 'hello'",
            "--provider",
            "lmstudio",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        // Isolate config/DB so this doesn't interfere with real sessions.
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Find the JSON object in stdout (skip any non-JSON lines like banners)
    let json_str = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("No JSON in stdout.\nstdout: {stdout}\nstderr: {stderr}"));

    let json: serde_json::Value = serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("Invalid JSON: {e}\nline: {json_str}\nstderr: {stderr}"));

    assert_eq!(
        json["success"], true,
        "Expected success.\nJSON: {json}\nstderr: {stderr}"
    );
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        !response.is_empty(),
        "Response should not be empty.\nJSON: {json}\nstderr: {stderr}"
    );
}

#[test]
#[ignore]
fn test_headless_tool_use() {
    if !lmstudio_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();

    // Create a file for the model to read
    std::fs::write(tmp.path().join("hello.txt"), "test content 12345").unwrap();

    let output = Command::new(koda_bin())
        .args([
            "-p",
            "Read the file hello.txt and tell me what it contains. Be brief.",
            "--provider",
            "lmstudio",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let json_str = stdout
        .lines()
        .find(|line| line.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("No JSON in stdout.\nstdout: {stdout}\nstderr: {stderr}"));

    let json: serde_json::Value = serde_json::from_str(json_str)
        .unwrap_or_else(|e| panic!("Invalid JSON: {e}\nline: {json_str}\nstderr: {stderr}"));

    assert_eq!(
        json["success"], true,
        "Expected success.\nJSON: {json}\nstderr: {stderr}"
    );
}

#[test]
#[ignore]
fn test_headless_session_resume() {
    if !lmstudio_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();

    // Run first prompt — capture session ID from JSON output.
    let output1 = Command::new(koda_bin())
        .args([
            "-p",
            "Say 'alpha'",
            "--provider",
            "lmstudio",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("Failed to run koda (turn 1)");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let json1_str = stdout1
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .expect("No JSON in turn 1");
    let json1: serde_json::Value = serde_json::from_str(json1_str).expect("Bad JSON turn 1");
    let session_id = json1["session_id"].as_str().expect("No session_id in JSON");

    // Run second prompt resuming the same session.
    let output2 = Command::new(koda_bin())
        .args([
            "-p",
            "Say 'beta'",
            "--provider",
            "lmstudio",
            "--output-format",
            "json",
            "--resume",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("Failed to run koda (turn 2)");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let stderr2 = String::from_utf8_lossy(&output2.stderr);

    let json2_str = stdout2
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("No JSON in turn 2.\nstdout: {stdout2}\nstderr: {stderr2}"));
    let json2: serde_json::Value = serde_json::from_str(json2_str).expect("Bad JSON turn 2");

    assert_eq!(json2["success"], true);
    assert_eq!(
        json2["session_id"].as_str().unwrap(),
        session_id,
        "Resumed session should keep the same ID"
    );
}
