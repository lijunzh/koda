//! Integration tests for the koda-ast MCP server.
//!
//! Spawns the `koda-ast` binary, speaks JSON-RPC over stdin/stdout, and
//! verifies the protocol surface.
//!
//! ## Why one big smoke test
//!
//! Earlier this file had four `#[tokio::test]`s, each spawning its own
//! subprocess via `tokio::process::Command`. That pattern was flaky on
//! macOS CI runners (cancelled at 14+ min on PRs #1086/#1087). Root cause:
//! tokio's process driver on macOS races on pipe wake-ups; multiple
//! subprocesses per test binary compounded it. See
//! `koda-cli/tests/server_test.rs` for the full investigation.
//!
//! Today: one subprocess for the whole protocol smoke-test, std::process
//! plus a dedicated reader thread, no tokio runtime in the test layer.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// Bound any single JSON-RPC round-trip. AST analysis is fast; 15 s leaves
/// head-room for cold macOS spawn without letting a wedge eat CI budget.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);

fn koda_ast_binary() -> String {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_koda-ast") {
        return path;
    }
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

/// A running MCP server subprocess plus a background reader thread that
/// funnels response lines through an mpsc channel.
struct StdioMcpHarness {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<std::io::Result<String>>,
    reader_handle: Option<thread::JoinHandle<()>>,
    stderr_path: std::path::PathBuf,
}

impl StdioMcpHarness {
    fn spawn(stderr_dir: &tempfile::TempDir) -> Self {
        let stderr_path = stderr_dir.path().join("koda-ast-stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

        let binary = koda_ast_binary();
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            .env("RUST_LOG", "rmcp=trace,koda_ast=debug")
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to start {binary}: {e}"));

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

    fn dump_stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_else(|e| format!("<read err: {e}>"))
    }

    /// Send a JSON-RPC request and wait for a single response line. For
    /// notifications (no `id`), use `send_notification` instead.
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
                     response (process exited: {status:?}).\nSent: {}\n=== server stderr ===\n{stderr}",
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
                     {status:?}).\nSent: {}\n=== server stderr ===\n{stderr}",
                    serde_json::to_string_pretty(msg).unwrap()
                );
            }
        };

        if response.trim().is_empty() {
            panic!("[{label}] Server returned empty response");
        }

        serde_json::from_str(response.trim())
            .unwrap_or_else(|e| panic!("[{label}] invalid JSON: {e}\nraw: {response:?}"))
    }

    fn send_notification(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin gone");
        let line = serde_json::to_string(msg).unwrap() + "\n";
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        // Settle so the server's async stdin reader (rmcp uses tokio,
        // which races on macOS pipe wake-ups) processes the notification
        // before our next request races ahead. 250 ms was chosen empirically:
        // 50 ms still produced ~17% flake under stress, 250 ms hit 0% in
        // local 60-iteration runs. Cheap insurance, never on the hot path.
        thread::sleep(Duration::from_millis(250));
    }

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

impl Drop for StdioMcpHarness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn initialize_msg(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0", "id": id, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.1" }
        }
    })
}

/// End-to-end smoke test of the koda-ast MCP server. Walks through:
///   1. `initialize` returns serverInfo with name=koda-ast,
///   2. `notifications/initialized` is accepted,
///   3. `tools/list` includes the AstAnalysis tool,
///   4. `tools/call AstAnalysis analyze_file` on a real file finds functions,
///   5. `tools/call AstAnalysis analyze_file` on a missing file returns isError.
///
/// One subprocess for the whole flow — see module docs for why.
///
/// Wrapped in a one-shot retry: rmcp's tokio-based stdio transport has a
/// rare race on macOS pipe wake-ups (~3 % flake locally) where a queued
/// response never reaches the OS pipe. The test is fully idempotent (fresh
/// subprocess per attempt), so retrying once collapses effective flake to
/// ~0.1 % — well below CI noise. If both attempts fail, the *second*
/// panic surfaces with full diagnostics so we still see what broke.
#[test]
fn test_mcp_protocol_smoke() {
    run_with_retry("mcp_protocol_smoke", run_smoke_test);
}

/// Execute `f` and retry once on panic. The second attempt is allowed to
/// panic normally so the test framework reports a real failure with the
/// actual diagnostic message (stderr dump, etc.).
fn run_with_retry(label: &str, f: fn()) {
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if first.is_ok() {
        return;
    }
    eprintln!("[{label}] first attempt failed; retrying once...");
    f();
}

fn run_smoke_test() {
    let stderr_dir = tempfile::TempDir::new().unwrap();
    let mut srv = StdioMcpHarness::spawn(&stderr_dir);

    // 1. initialize
    let resp = srv.send_and_recv("initialize", &initialize_msg(1));
    assert_eq!(
        resp["result"]["serverInfo"]["name"], "koda-ast",
        "initialize: serverInfo.name should be 'koda-ast'"
    );
    assert!(
        resp["result"]["capabilities"]["tools"].is_object(),
        "initialize: capabilities.tools should be an object"
    );

    // 2. notifications/initialized (no response expected)
    srv.send_notification(&json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    }));

    // 3. tools/list
    let resp = srv.send_and_recv(
        "tools/list",
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    );
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools/list: result.tools should be an array");
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        tool_names.contains(&"AstAnalysis"),
        "tools/list: should include AstAnalysis, got: {tool_names:?}"
    );

    // 4. analyze a real Rust file
    let tmp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    std::fs::write(tmp.path(), "fn main() {}\nfn helper() {}").unwrap();
    let resp = srv.send_and_recv(
        "tools/call analyze_file (valid)",
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
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("analyze_file (valid): content[0].text should be a string");
    assert!(
        text.contains("main"),
        "should find main function in: {text}"
    );
    assert!(
        text.contains("helper"),
        "should find helper function in: {text}"
    );

    // 5. analyze a nonexistent file → should return isError, not crash
    let resp = srv.send_and_recv(
        "tools/call analyze_file (missing)",
        &json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {
                "name": "AstAnalysis",
                "arguments": {
                    "action": "analyze_file",
                    "file_path": "/nonexistent/file.rs"
                }
            }
        }),
    );
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        is_error,
        "analyze_file (missing): should return isError=true, got: {resp}"
    );

    srv.shutdown();
}
