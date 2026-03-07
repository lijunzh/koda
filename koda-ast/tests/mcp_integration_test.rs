//! Integration tests for koda-ast MCP server.
//!
//! Spawns the koda-ast binary, sends MCP JSON-RPC messages over stdio,
//! and verifies responses. These replace manual tests 1-3 from
//! tests/manual/auto-provision-mcp.md.
//!
//! Requires: `cargo build -p koda-ast` before running.

use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Find the koda-ast binary.
fn koda_ast_binary() -> String {
    // cargo sets this env var for integration tests when using [[bin]]
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_koda-ast") {
        return path;
    }
    // Fallback: search target directory
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace = std::path::Path::new(manifest_dir).parent().unwrap();
    for profile in ["debug", "release"] {
        let path = workspace.join("target").join(profile).join("koda-ast");
        if path.exists() {
            return path.to_string_lossy().to_string();
        }
    }
    panic!("koda-ast binary not found. Run `cargo build -p koda-ast` first.");
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

    // Read response (only for requests with "id")
    if msg.get("id").is_some() {
        let mut response = String::new();
        stdout.read_line(&mut response).await.unwrap();
        Some(serde_json::from_str(&response).unwrap())
    } else {
        // Notification — no response expected, small delay for processing
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        None
    }
}

/// Start the MCP server and initialize it.
async fn start_server() -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    let binary = koda_ast_binary();
    let mut child = Command::new(&binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to start {binary}: {e}"));

    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

// ── Test 1: MCP Initialize ─────────────────────────────────

#[tokio::test]
async fn test_mcp_initialize() {
    let (mut child, mut stdin, mut stdout) = start_server().await;

    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await
    .unwrap();

    let result = &resp["result"];
    assert_eq!(result["serverInfo"]["name"], "koda-ast");
    assert!(result["capabilities"]["tools"].is_object());

    drop(stdin);
    let _ = child.kill().await;
}

// ── Test 2: Tools List ──────────────────────────────────────

#[tokio::test]
async fn test_mcp_tools_list() {
    let (mut child, mut stdin, mut stdout) = start_server().await;

    // Initialize
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await;

    // Send initialized notification
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // List tools
    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap();

    let tools = resp["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty(), "Should have at least one tool");

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        tool_names.contains(&"AstAnalysis"),
        "Should include AstAnalysis, got: {tool_names:?}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

// ── Test 3: Analyze File ────────────────────────────────────

#[tokio::test]
async fn test_mcp_analyze_file() {
    // Create a test file
    let tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    std::fs::write(tmp.path(), "fn main() {}\nfn helper() {}").unwrap();

    let (mut child, mut stdin, mut stdout) = start_server().await;

    // Initialize + notification
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await;
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // Call AstAnalysis
    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "AstAnalysis",
                "arguments": {
                    "action": "analyze_file",
                    "file_path": tmp.path().to_str().unwrap()
                }
            }
        }),
    )
    .await
    .unwrap();

    let content = &resp["result"]["content"][0]["text"];
    let text = content.as_str().unwrap();
    assert!(text.contains("main"), "Should find main function: {text}");
    assert!(
        text.contains("helper"),
        "Should find helper function: {text}"
    );

    drop(stdin);
    let _ = child.kill().await;
}

// ── Test 3b: File Not Found ─────────────────────────────────

#[tokio::test]
async fn test_mcp_file_not_found() {
    let (mut child, mut stdin, mut stdout) = start_server().await;

    // Initialize + notification
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.1" }
            }
        }),
    )
    .await;
    send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    // Call with nonexistent file
    let resp = send_and_receive(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "AstAnalysis",
                "arguments": {
                    "action": "analyze_file",
                    "file_path": "/nonexistent/file.rs"
                }
            }
        }),
    )
    .await
    .unwrap();

    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(is_error, "Should return error for nonexistent file");

    drop(stdin);
    let _ = child.kill().await;
}
