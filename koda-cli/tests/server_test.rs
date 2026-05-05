//! ACP server integration tests.
//!
//! Tests that the `koda server --stdio` subprocess handles JSON-RPC
//! messages correctly over stdin/stdout.
//!
//! ## Why this is one big test, not N small ones
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
//! subprocess for the whole protocol coverage. Each protocol scenario lives
//! in its own `step_*` helper so failure attribution is still readable, and
//! we get linear test time + zero spawn flakes. (#1264 expansion: this file
//! went from one smoke step to eight ordered protocol scenarios; keeping
//! them in one subprocess avoids reintroducing the original flake.)

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

    /// Send a raw line of bytes (no JSON serialization). Used to verify the
    /// server is resilient to malformed input — the protocol contract is
    /// that an invalid line gets a `-32700`/`-32600` error response and the
    /// server keeps running, rather than crashing the whole stdio loop.
    fn send_raw_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin gone");
        stdin.write_all(line.as_bytes()).unwrap();
        if !line.ends_with('\n') {
            stdin.write_all(b"\n").unwrap();
        }
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

/// End-to-end coverage of the ACP stdio server protocol. Walks through every
/// scenario the server is expected to handle correctly without an LLM in the
/// loop. Scenario list (in execution order — ordering matters because some
/// steps depend on session state established by earlier ones):
///
///   1. `initialize`                                  — happy path, agent info
///   2. `unknown_method` request                      — -32601 error
///   3. `prompt_without_active_session`               — -32000 error
///   4. `cancel_notification_without_active_session`  — no-op, server alive
///   5. `authenticate`                                — no-op response
///   6. `new_session`                                 — happy path, sessionId
///   7. `new_session_replaces_active`                 — second call, distinct id
///   8. `cancel_notification` (active session)        — no response, no crash
///   9. `malformed_json_line`                         — -32700 parse error
///  10. `post_recovery_initialize`                    — server still responsive
///
/// One subprocess for the whole flow — see module docs for why.
#[test]
fn test_server_protocol_e2e() {
    let project_dir = tempfile::TempDir::new().unwrap();
    let config_dir = tempfile::TempDir::new().unwrap();
    let mut srv = ServerHarness::spawn(&project_dir, &config_dir);

    step_initialize(&mut srv, 1);
    step_unknown_method_returns_method_not_found(&mut srv, 2);
    step_prompt_without_active_session_returns_error(&mut srv, 3);
    step_cancel_notification_without_active_session_is_noop(&mut srv);
    step_authenticate_succeeds(&mut srv, 4);
    let session_id_a = step_new_session(&mut srv, 5, &project_dir);
    let session_id_b = step_new_session_replaces_active(&mut srv, 6, &project_dir, &session_id_a);
    step_cancel_notification_for_active_session(&mut srv, &session_id_b);
    step_malformed_json_line_returns_parse_error(&mut srv);
    step_post_recovery_initialize(&mut srv, 99);

    srv.shutdown();
}

// ── step helpers ─────────────────────────────────────────
//
// Each helper is one self-contained protocol scenario. They take `&mut srv`
// (sharing the spawned subprocess across the whole `#[test]`) and an
// explicit JSON-RPC `id` so the caller controls id allocation — makes the
// expected ids in the body of `test_server_protocol_e2e` greppable.

fn step_initialize(srv: &mut ServerHarness, id: u64) {
    let resp = srv.send_and_recv("initialize", &initialize_msg(id));
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], id);
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
}

/// JSON-RPC -32601 (Method not found) for any method the ACP decoder doesn't
/// know. Guards `handle_request`'s decode-error branch (server.rs:214).
fn step_unknown_method_returns_method_not_found(srv: &mut ServerHarness, id: u64) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "nonsense/method",
        "params": {}
    });
    let resp = srv.send_and_recv("unknown_method", &req);
    assert_eq!(resp["id"], id, "unknown_method: id should round-trip");
    assert_eq!(
        resp["error"]["code"], -32601,
        "unknown_method: expected -32601 (Method not found), got: {resp:?}"
    );
    let err_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        err_msg.contains("nonsense/method"),
        "unknown_method: error message should name the offending method, got: {err_msg:?}"
    );
}

/// `session/prompt` before any `session/new` must return -32000 with the
/// helpful "Call session/new first" message rather than panicking or hanging.
/// Guards `handle_prompt`'s no-active-session branch (server.rs:361).
fn step_prompt_without_active_session_returns_error(srv: &mut ServerHarness, id: u64) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": "nonexistent",
            "prompt": [{"type": "text", "text": "hello"}]
        }
    });
    let resp = srv.send_and_recv("prompt_without_session", &req);
    assert_eq!(resp["id"], id);
    assert_eq!(
        resp["error"]["code"], -32000,
        "prompt_without_session: expected -32000, got: {resp:?}"
    );
    let err_msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        err_msg.contains("session/new"),
        "prompt_without_session: error should suggest calling session/new, got: {err_msg:?}"
    );
}

/// `session/cancel` is a notification (no `id`, no response expected). When
/// no session is active, `handle_notification` early-exits via the
/// `Some(ref active) = state.active` guard. We can't observe a response
/// directly — we verify by sending a follow-up request and confirming the
/// server still answers.
fn step_cancel_notification_without_active_session_is_noop(srv: &mut ServerHarness) {
    srv.send_notification(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": "nonexistent" }
    }));
    // The aliveness check happens when the next step sends a request. If the
    // server crashed here, that step's recv would time out / hit EOF.
}

/// `authenticate` is a no-op for the local agent but still returns a valid
/// response so clients that always call it during handshake don't break.
/// Guards `handle_authenticate` (server.rs:294).
fn step_authenticate_succeeds(srv: &mut ServerHarness, id: u64) {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "authenticate",
        "params": { "methodId": "local" }
    });
    let resp = srv.send_and_recv("authenticate", &req);
    assert_eq!(resp["id"], id);
    assert!(
        resp["result"].is_object(),
        "authenticate: expected result object even though it's a no-op, got: {resp:?}"
    );
    // No assertion on result body shape — AuthenticateResponse is currently
    // empty by design. If it ever sprouts fields we'll add field assertions
    // when we wire authenticated transports.
}

fn step_new_session(srv: &mut ServerHarness, id: u64, project_dir: &tempfile::TempDir) -> String {
    let resp = srv.send_and_recv("session/new", &new_session_msg(id, project_dir));
    assert_eq!(resp["id"], id);
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
    session_id
}

/// The server's `ServerState.active` field is a single `Option<ActiveSession>`
/// (server.rs:69) — calling `session/new` again silently replaces the previous
/// session rather than returning an error. We document that contract here:
/// the second call must succeed and return a *distinct* sessionId. If we ever
/// add multi-session support, this test will need to flip to asserting both
/// sessions remain accessible.
fn step_new_session_replaces_active(
    srv: &mut ServerHarness,
    id: u64,
    project_dir: &tempfile::TempDir,
    previous_session_id: &str,
) -> String {
    let session_id = step_new_session(srv, id, project_dir);
    assert_ne!(
        session_id, previous_session_id,
        "session/new called twice should return distinct sessionIds (got the same: {session_id})"
    );
    session_id
}

fn step_cancel_notification_for_active_session(srv: &mut ServerHarness, session_id: &str) {
    srv.send_notification(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id }
    }));
    // Liveness verified by `step_post_recovery_initialize` later in the flow.
}

/// A non-JSON line on stdin must produce a -32700 parse-error response with
/// `id: null` and the server must keep running. Guards the `serde_json::from_str`
/// branch in the main loop (server.rs:166).
fn step_malformed_json_line_returns_parse_error(srv: &mut ServerHarness) {
    srv.send_raw_line("not even close to JSON {{{{");
    let response = match srv.rx.recv_timeout(RPC_TIMEOUT) {
        Ok(Ok(line)) => line,
        other => panic!("malformed_json: expected a parse-error response line, got: {other:?}"),
    };
    let resp: Value = serde_json::from_str(response.trim()).unwrap_or_else(|e| {
        panic!("malformed_json: invalid JSON in response: {e}\nraw: {response:?}")
    });
    assert_eq!(
        resp["error"]["code"], -32700,
        "malformed_json: expected -32700 (Parse error), got: {resp:?}"
    );
    assert!(
        resp["id"].is_null(),
        "malformed_json: parse-error response must use null id (the offending line had no parseable id), got: {:?}",
        resp["id"]
    );
}

/// Final liveness check: after every weird thing we threw at the server
/// (unknown method, prompt without session, cancel-without-session, malformed
/// JSON, etc.) it must still answer a vanilla `initialize` request. Catches
/// silent crashes / wedges in any of the earlier steps that wouldn't be
/// detected by their own assertions.
fn step_post_recovery_initialize(srv: &mut ServerHarness, id: u64) {
    let resp = srv.send_and_recv("post_recovery_initialize", &initialize_msg(id));
    assert_eq!(resp["id"], id);
    assert!(
        resp["result"].is_object(),
        "post_recovery_initialize: server should still respond after malformed input + cancels, got: {resp:?}"
    );
}
