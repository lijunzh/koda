//! ACP server integration tests.
//!
//! Tests that the `koda server --stdio` subprocess handles JSON-RPC
//! messages correctly over stdin/stdout.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

static TEST_MUTEX: Mutex<()> = Mutex::const_new(());

/// Get the path to the built binary.
fn koda_bin() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_koda") {
        return path;
    }

    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// Send a JSON-RPC message to the server's stdin and read the response line.
/// Panics with diagnostic info if the server process exits before responding.
///
/// Times out after 30 seconds so slow macOS subprocess startup never hangs CI
/// indefinitely, but genuinely wedged servers still fail with a bounded error.
async fn send_and_recv(
    child: &mut tokio::process::Child,
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    msg: &Value,
) -> Value {
    let line = serde_json::to_string(msg).unwrap() + "\n";
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();

    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        stdout.read_line(&mut response),
    )
    .await
    .expect("timed out waiting for server response (30 s)")
    .unwrap();

    if response.trim().is_empty() {
        // Server likely crashed — collect exit status for diagnostics.
        let status = child.try_wait().ok().flatten();
        panic!(
            "Server returned empty response (process exited: {:?}). Sent: {}",
            status,
            serde_json::to_string_pretty(msg).unwrap()
        );
    }

    serde_json::from_str(response.trim()).unwrap()
}

/// Send a JSON-RPC notification (no response expected).
async fn send_notification(stdin: &mut tokio::process::ChildStdin, msg: &Value) {
    let line = serde_json::to_string(msg).unwrap() + "\n";
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

fn initialize_msg() -> Value {
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
) -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    let mut child = Command::new(koda_bin())
        .arg("--project-root")
        .arg(project_dir.path())
        .args(["server", "--stdio"])
        // Isolate DB per test — config_dir() reads XDG_CONFIG_HOME
        .env("XDG_CONFIG_HOME", config_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start koda server");

    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

#[tokio::test]
async fn test_server_initialize() {
    let _guard = TEST_MUTEX.lock().await;
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let (mut child, mut stdin, mut stdout) = spawn_server(&project_dir, &config_dir);

    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg()).await;

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

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_server_new_session() {
    let _guard = TEST_MUTEX.lock().await;
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let (mut child, mut stdin, mut stdout) = spawn_server(&project_dir, &config_dir);

    // Initialize first
    let _init_resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg()).await;

    // Create new session
    let new_session = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": project_dir.path().to_string_lossy(),
            "mcpServers": []
        }
    });
    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &new_session).await;

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

    drop(stdin);
    let _ = child.kill().await;
}

#[tokio::test]
async fn test_server_cancel_notification() {
    let _guard = TEST_MUTEX.lock().await;
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let (mut child, mut stdin, mut stdout) = spawn_server(&project_dir, &config_dir);

    // Initialize
    let _init_resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &initialize_msg()).await;

    // Create session
    let new_session = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": project_dir.path().to_string_lossy(),
            "mcpServers": []
        }
    });
    let resp = send_and_recv(&mut child, &mut stdin, &mut stdout, &new_session).await;
    let session_id = resp["result"]["sessionId"].as_str().unwrap();

    // Send cancel notification (no id = notification, should not crash)
    let cancel = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {
            "sessionId": session_id
        }
    });
    send_notification(&mut stdin, &cancel).await;

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
    let resp2 = send_and_recv(&mut child, &mut stdin, &mut stdout, &init2).await;
    assert_eq!(resp2["id"], 3);
    assert!(resp2["result"].is_object());

    drop(stdin);
    let _ = child.kill().await;
}
