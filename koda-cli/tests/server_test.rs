//! ACP server integration tests.
//!
//! Tests that the `koda server --stdio` subprocess handles JSON-RPC
//! messages correctly over stdin/stdout.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Get the path to the built binary.
fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// Send a JSON-RPC message to the server's stdin and read the response line.
/// Panics with diagnostic info if the server process exits before responding.
fn send_and_recv(
    child: &mut std::process::Child,
    stdin: &mut impl Write,
    stdout: &mut impl BufRead,
    msg: &serde_json::Value,
) -> serde_json::Value {
    let line = serde_json::to_string(msg).unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();

    let mut response = String::new();
    stdout.read_line(&mut response).unwrap();

    if response.trim().is_empty() {
        // Server likely crashed — collect exit status for diagnostics
        let status = child.try_wait().ok().flatten();
        panic!(
            "Server returned empty response (process exited: {:?}). \
             Sent: {}",
            status,
            serde_json::to_string_pretty(msg).unwrap()
        );
    }

    serde_json::from_str(response.trim()).unwrap()
}

/// Send a JSON-RPC notification (no response expected).
fn send_notification(stdin: &mut impl Write, msg: &serde_json::Value) {
    let line = serde_json::to_string(msg).unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
}

fn initialize_msg() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "0.1",
            "clientCapabilities": {}
        }
    })
}

/// Spawn the koda server process in a temp directory with isolated config/DB.
fn spawn_server(
    project_dir: &tempfile::TempDir,
    config_dir: &tempfile::TempDir,
) -> std::process::Child {
    Command::new(koda_bin())
        .arg("--project-root")
        .arg(project_dir.path())
        .args(["server", "--stdio"])
        // Isolate DB per test — config_dir() reads XDG_CONFIG_HOME
        .env("XDG_CONFIG_HOME", config_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start koda server")
}

#[test]
fn test_server_initialize() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let mut child = spawn_server(&project_dir, &config_dir);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg());

    // Verify response structure
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(resp["result"].is_object(), "Expected result object");

    // Verify agent info (ACP uses camelCase)
    let agent_info = &resp["result"]["agentInfo"];
    assert_eq!(agent_info["name"], "koda", "Agent name should be 'koda'");
    assert_eq!(
        agent_info["version"],
        env!("CARGO_PKG_VERSION"),
        "Should have correct version"
    );

    // Clean up
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn test_server_new_session() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let mut child = spawn_server(&project_dir, &config_dir);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Initialize first
    let _init_resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg());

    // Create new session
    let new_session = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": project_dir.path().to_string_lossy(),
            "mcpServers": []  // Required by ACP protocol schema
        }
    });
    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &new_session);

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    assert!(resp["result"].is_object(), "Expected result object");

    // ACP uses camelCase: sessionId
    let session_id = &resp["result"]["sessionId"];
    assert!(session_id.is_string(), "Expected sessionId in response");
    assert!(
        !session_id.as_str().unwrap().is_empty(),
        "sessionId should not be empty"
    );

    // Clean up
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn test_server_cancel_notification() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let mut child = spawn_server(&project_dir, &config_dir);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    // Initialize
    let _init_resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg());

    // Create session
    let new_session = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": project_dir.path().to_string_lossy(),
            "mcpServers": []  // Required by ACP protocol schema
        }
    });
    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &new_session);
    let session_id = resp["result"]["sessionId"].as_str().unwrap();

    // Send cancel notification (no id = notification, should not crash)
    let cancel = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {
            "sessionId": session_id
        }
    });
    send_notification(&mut stdin, &cancel);

    // Server should still be responsive after cancel
    let init2 = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "initialize",
        "params": {
            "protocolVersion": "0.1",
            "clientCapabilities": {}
        }
    });
    let resp2 = send_and_recv(&mut child, &mut stdin, &mut stdout, &init2);
    assert_eq!(resp2["id"], 3);
    assert!(resp2["result"].is_object());

    // Clean up
    drop(stdin);
    let _ = child.wait();
}
