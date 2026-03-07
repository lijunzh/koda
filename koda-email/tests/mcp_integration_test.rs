//! Integration tests for koda-email MCP server.
//!
//! Layer 1 (always run): MCP protocol tests — initialize, tools/list,
//! graceful error handling when credentials are not configured.
//!
//! Layer 2 (#[ignore]): Live email tests — only run when KODA_EMAIL_*
//! environment variables are set. Opt-in via:
//!   cargo test -p koda-email -- --ignored
//!
//! Requires: `cargo build -p koda-email` before running.

use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

// ── Helpers ─────────────────────────────────────────────────

/// Find the koda-email binary.
fn koda_email_binary() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_koda-email") {
        return path;
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace = std::path::Path::new(manifest_dir).parent().unwrap();
    for profile in ["debug", "release"] {
        let path = workspace.join("target").join(profile).join("koda-email");
        if path.exists() {
            return path.to_string_lossy().to_string();
        }
    }
    panic!("koda-email binary not found. Run `cargo build -p koda-email` first.");
}

/// Send a JSON-RPC message and read the response.
async fn send_and_receive(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    msg: &Value,
) -> Option<Value> {
    let line = serde_json::to_string(msg).unwrap() + "\n";
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    if msg.get("id").is_some() {
        let mut response = String::new();
        stdout.read_line(&mut response).await.unwrap();
        Some(serde_json::from_str(&response).unwrap())
    } else {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        None
    }
}

/// Start the MCP server and return handles.
async fn start_server() -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    let binary = koda_email_binary();
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Ensure no accidental credentials leak into tests
        .env_remove("KODA_EMAIL_IMAP_HOST")
        .env_remove("KODA_EMAIL_USERNAME")
        .env_remove("KODA_EMAIL_PASSWORD")
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to start {binary}: {e}"));

    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

/// Initialize + send notifications/initialized.
async fn initialize(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
) -> Value {
    let resp = send_and_receive(
        stdin,
        stdout,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await
    .unwrap();

    send_and_receive(
        stdin,
        stdout,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    resp
}

// ── Layer 1: MCP Protocol Tests (always run) ────────────────

#[tokio::test]
async fn test_mcp_initialize() {
    let (mut child, mut stdin, mut stdout) = start_server().await;

    let resp = initialize(&mut stdin, &mut stdout).await;
    let result = &resp["result"];

    assert_eq!(result["serverInfo"]["name"], "koda-email");
    assert!(result["capabilities"]["tools"].is_object());

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_mcp_tools_list() {
    let (mut child, mut stdin, mut stdout) = start_server().await;
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(
        tool_names.contains(&"EmailRead"),
        "Should include EmailRead, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"EmailSend"),
        "Should include EmailSend, got: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"EmailSearch"),
        "Should include EmailSearch, got: {tool_names:?}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_tool_schemas_have_descriptions() {
    let (mut child, mut stdin, mut stdout) = start_server().await;
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"].is_string(),
            "{name} should have a description"
        );
        assert!(
            tool["inputSchema"].is_object(),
            "{name} should have an inputSchema"
        );
    }

    drop(stdin);
    let _ = child.kill().await;
}

// ── Layer 1: Graceful Degradation (no credentials) ──────────

#[tokio::test]
async fn test_email_read_without_credentials_returns_setup_instructions() {
    let (mut child, mut stdin, mut stdout) = start_server().await;
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailRead",
                "arguments": { "count": 5 }
            }
        }),
    )
    .await
    .unwrap();

    // Should return an error with setup instructions, not crash
    let error = &resp["error"];
    assert!(error.is_object(), "Should return JSON-RPC error: {resp}");
    let message = error["message"].as_str().unwrap_or("");
    assert!(
        message.contains("KODA_EMAIL"),
        "Error should mention KODA_EMAIL env vars: {message}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_email_send_without_credentials_returns_setup_instructions() {
    let (mut child, mut stdin, mut stdout) = start_server().await;
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailSend",
                "arguments": {
                    "to": "test@example.com",
                    "subject": "Test",
                    "body": "Hello"
                }
            }
        }),
    )
    .await
    .unwrap();

    let error = &resp["error"];
    assert!(error.is_object(), "Should return JSON-RPC error: {resp}");
    let message = error["message"].as_str().unwrap_or("");
    assert!(
        message.contains("KODA_EMAIL"),
        "Error should mention KODA_EMAIL env vars: {message}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_email_search_without_credentials_returns_setup_instructions() {
    let (mut child, mut stdin, mut stdout) = start_server().await;
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailSearch",
                "arguments": { "query": "test" }
            }
        }),
    )
    .await
    .unwrap();

    let error = &resp["error"];
    assert!(error.is_object(), "Should return JSON-RPC error: {resp}");
    let message = error["message"].as_str().unwrap_or("");
    assert!(
        message.contains("KODA_EMAIL"),
        "Error should mention KODA_EMAIL env vars: {message}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

// ── Layer 1: Version flag ───────────────────────────────────

#[tokio::test]
async fn test_version_flag() {
    let binary = koda_email_binary();
    let output = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("koda-email"),
        "--version should print 'koda-email': {stdout}"
    );
}

// ── Layer 2: Live Email Tests (opt-in, #[ignore]) ───────────
//
// These tests require real IMAP/SMTP credentials.
// Run with: cargo test -p koda-email -- --ignored
//
// Set these env vars first:
//   KODA_EMAIL_IMAP_HOST=imap.gmail.com
//   KODA_EMAIL_USERNAME=you@gmail.com
//   KODA_EMAIL_PASSWORD=your-app-password

/// Start server WITH credentials from env (for live tests).
async fn start_server_with_creds() -> Option<(
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
)> {
    // Check that credentials are available
    if std::env::var("KODA_EMAIL_IMAP_HOST").is_err()
        || std::env::var("KODA_EMAIL_USERNAME").is_err()
        || std::env::var("KODA_EMAIL_PASSWORD").is_err()
    {
        eprintln!("Skipping: KODA_EMAIL_* env vars not set");
        return None;
    }

    let binary = koda_email_binary();
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to start {binary}: {e}"));

    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    Some((child, stdin, stdout))
}

#[tokio::test]
#[ignore]
async fn test_live_email_read() {
    let Some((mut child, mut stdin, mut stdout)) = start_server_with_creds().await else {
        return;
    };
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailRead",
                "arguments": { "count": 3 }
            }
        }),
    )
    .await
    .unwrap();

    // Should succeed (not an error)
    assert!(
        resp["error"].is_null(),
        "EmailRead should succeed with valid creds: {resp}"
    );

    let content = &resp["result"]["content"][0]["text"];
    let text = content.as_str().unwrap_or("");
    // Even an empty inbox returns something meaningful
    assert!(
        !text.is_empty(),
        "Should return some output (emails or 'no emails')"
    );
    eprintln!("EmailRead result:\n{text}");

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
#[ignore]
async fn test_live_email_search() {
    let Some((mut child, mut stdin, mut stdout)) = start_server_with_creds().await else {
        return;
    };
    initialize(&mut stdin, &mut stdout).await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailSearch",
                "arguments": { "query": "test", "max_results": 3 }
            }
        }),
    )
    .await
    .unwrap();

    assert!(
        resp["error"].is_null(),
        "EmailSearch should succeed: {resp}"
    );

    let content = &resp["result"]["content"][0]["text"];
    let text = content.as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "Should return search results or 'no results'"
    );
    eprintln!("EmailSearch result:\n{text}");

    drop(stdin);
    let _ = child.kill().await;
}
