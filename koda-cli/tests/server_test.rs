//! ACP server integration tests.
//!
//! Tests that the `koda server --stdio` subprocess handles JSON-RPC
//! messages correctly over stdin/stdout.
//!
//! ## Why this is one big test, not three small ones
//!
//! Earlier revisions split this into separate `test_server_initialize`,
//! `test_server_new_session`, and `test_server_cancel_notification` tests,
//! each spawning its own `koda` subprocess. That harness was flaky on macOS
//! (~30 % at the cargo-test level locally; cancelled-after-14-min on CI in
//! runs #1086 / #1087) when multiple tests ran in the same test binary,
//! even with a serializing mutex and `std::process` (no tokio reactor).
//! Investigation showed:
//!   * `echo | koda server --stdio` from a shell: 30/30 successful (p95=155 ms)
//!   * One Rust test in its own process: 20/20 successful
//!   * Three Rust tests in one process: ~30 % flake rate
//!
//! Rather than chase the residual race, we use a single test that reuses one
//! subprocess for the whole protocol smoke-test. This is also a more honest
//! description of what's being tested (the same binary, the same protocol)
//! and removes a spawn-per-test overhead that bought us nothing.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// Bound any single JSON-RPC round-trip. Cold macOS startup is ~3 s in CI;
/// 30 s leaves head-room without letting a wedge eat the whole job budget.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Locate the built `koda` binary. Cargo provides this for integration tests.
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

/// A running `koda server --stdio` subprocess plus a background reader
/// thread that funnels response lines through an mpsc channel.
struct ServerHarness {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<std::io::Result<String>>,
    reader_handle: Option<thread::JoinHandle<()>>,
    stderr_path: std::path::PathBuf,
}

impl ServerHarness {
    fn spawn(project_dir: &tempfile::TempDir, config_dir: &tempfile::TempDir) -> Self {
        // Capture stderr to a file under the per-test config_dir so we can
        // dump it on failure. `Stdio::inherit` gets eaten by cargo, and
        // `Stdio::piped` + a drain task introduces tokio coupling that we're
        // intentionally avoiding here.
        let stderr_path = config_dir.path().join("koda-stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

        let mut child = Command::new(koda_bin())
            .arg("--project-root")
            .arg(project_dir.path())
            .args(["server", "--stdio"])
            // Isolate DB per test — db::config_dir() honors XDG_CONFIG_HOME.
            .env("XDG_CONFIG_HOME", config_dir.path())
            .env("RUST_LOG", "koda_core=debug,koda_cli=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .expect("Failed to start koda server");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = mpsc::channel();
        let reader_handle = thread::spawn(move || {
            let mut buf = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match buf.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if tx.send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        Self {
            child: Some(child),
            stdin: Some(stdin),
            rx,
            reader_handle: Some(reader_handle),
            stderr_path,
        }
    }

    /// Read the captured subprocess stderr for diagnostics.
    fn dump_stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_else(|e| format!("<read err: {e}>"))
    }

    /// Send a JSON-RPC request and wait for one response line, bounded.
    fn send_and_recv(&mut self, label: &str, msg: &Value) -> Value {
        let stdin = self.stdin.as_mut().expect("stdin gone");
        let line = serde_json::to_string(msg).unwrap() + "\n";
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();

        let response = match self.rx.recv_timeout(RPC_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => panic!("[{label}] read error from server: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let status = self
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten());
                let stderr = self.dump_stderr();
                panic!(
                    "[{label}] timed out after {RPC_TIMEOUT:?} waiting for server \
                     response (process exited: {status:?}).\nSent: {}\n=== koda stderr ===\n{stderr}",
                    serde_json::to_string_pretty(msg).unwrap()
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = self
                    .child
                    .as_mut()
                    .and_then(|c| c.try_wait().ok().flatten());
                let stderr = self.dump_stderr();
                panic!(
                    "[{label}] Server EOF before responding (process exited: \
                     {status:?}).\nSent: {}\n=== koda stderr ===\n{stderr}",
                    serde_json::to_string_pretty(msg).unwrap()
                );
            }
        };

        if response.trim().is_empty() {
            panic!(
                "[{label}] Server returned empty response.\nSent: {}",
                serde_json::to_string_pretty(msg).unwrap()
            );
        }

        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("[{label}] invalid JSON: {e}\nraw: {response:?}"))
    }

    fn send_notification(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin gone");
        let line = serde_json::to_string(msg).unwrap() + "\n";
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Cleanly close stdin, then signal+reap the child. Idempotent.
    fn shutdown(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(h) = self.reader_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ServerHarness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn initialize_msg(id: u64) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "0.1",
            "clientCapabilities": {}
        }
    })
}

fn new_session_msg(id: u64, project_dir: &tempfile::TempDir) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/new",
        "params": {
            "cwd": project_dir.path().to_string_lossy(),
            "mcpServers": []
        }
    })
}

/// Single end-to-end smoke test of the ACP stdio server. Walks through:
///   1. `initialize` returns expected agent info,
///   2. `session/new` returns a non-empty session id,
///   3. `session/cancel` notification (no id) does not crash the server,
///   4. server still answers a follow-up `initialize` after the cancel.
///
/// One subprocess for the whole flow — see module docs for why.
#[test]
fn test_server_protocol_smoke() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let mut srv = ServerHarness::spawn(&project_dir, &config_dir);

    // Step 1: initialize
    let resp = srv.send_and_recv("initialize", &initialize_msg(1));
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(
        resp["result"].is_object(),
        "initialize: expected result object"
    );
    let agent_info = &resp["result"]["agentInfo"];
    assert_eq!(
        agent_info["name"], "koda",
        "initialize: agent name should be 'koda'"
    );
    assert_eq!(
        agent_info["version"],
        env!("CARGO_PKG_VERSION"),
        "initialize: should report compiled version"
    );

    // Step 2: session/new
    let resp = srv.send_and_recv("session/new", &new_session_msg(2, &project_dir));
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    assert!(
        resp["result"].is_object(),
        "session/new: expected result object"
    );
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .expect("session/new: sessionId should be a string")
        .to_string();
    assert!(
        !session_id.is_empty(),
        "session/new: sessionId should not be empty"
    );

    // Step 3: cancel notification (no id, no response expected)
    srv.send_notification(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id }
    }));

    // Step 4: server is still responsive after a cancel notification.
    let resp = srv.send_and_recv("post-cancel initialize", &initialize_msg(3));
    assert_eq!(resp["id"], 3);
    assert!(
        resp["result"].is_object(),
        "post-cancel initialize: expected result object"
    );

    srv.shutdown();
}
