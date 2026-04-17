//! Integration tests for the koda-email MCP server.
//!
//! Layer 1 (always run): one consolidated smoke test that walks through the
//! MCP protocol — initialize, tools/list, schema validation, and graceful
//! degradation when no credentials are configured.
//!
//! Layer 2 (`#[ignore]`d): live email tests against a real IMAP/SMTP account.
//! Each is independently opt-in via `cargo test -- --ignored`. Set:
//!   KODA_EMAIL_IMAP_HOST=imap.gmail.com
//!   KODA_EMAIL_USERNAME=you@gmail.com
//!   KODA_EMAIL_PASSWORD=your-app-password
//!
//! ## Why one big smoke test for layer 1
//!
//! Earlier this file had seven `#[tokio::test]`s, each spawning its own
//! subprocess via `tokio::process::Command`. That pattern was flaky on
//! macOS CI runners (cancelled at 14+ min on PRs #1086/#1087). Root cause:
//! tokio's process driver on macOS races on pipe wake-ups; multiple
//! subprocesses per test binary compounded it. See
//! `koda-cli/tests/server_test.rs` for the full investigation.
//!
//! Today: one subprocess for layer 1, std::process plus a dedicated reader
//! thread, no tokio runtime in the test layer. Live tests get the same
//! harness but stay as separate `#[ignore]`d tests so they can be invoked
//! individually.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

/// Bound any single JSON-RPC round-trip. Live IMAP/SMTP can take a few
/// seconds; 30 s is generous without letting a wedge eat CI budget.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Customization for `StdioMcpHarness::spawn`. Lets each test set up the
/// subprocess environment (clear creds for the no-creds smoke test, pass
/// them through for live tests) without copy-pasting the spawn boilerplate.
type CommandConfig = dyn FnOnce(&mut Command);

struct StdioMcpHarness {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: mpsc::Receiver<std::io::Result<String>>,
    reader_handle: Option<thread::JoinHandle<()>>,
    stderr_path: std::path::PathBuf,
}

impl StdioMcpHarness {
    fn spawn(stderr_dir: &tempfile::TempDir, configure: Box<CommandConfig>) -> Self {
        let stderr_path = stderr_dir.path().join("koda-email-stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

        let binary = koda_email_binary();
        let mut cmd = Command::new(&binary);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file))
            // rmcp=trace nudges the macOS reactor with extra I/O events;
            // empirically reduces a rare flake (~3 %) where a queued
            // response never wakes our reader. See `run_with_retry` below.
            .env("RUST_LOG", "rmcp=trace,koda_email=info");
        configure(&mut cmd);

        let mut child = cmd
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
                    Ok(0) => break,
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
        // before our next request races ahead. 250 ms was chosen empirically
        // to minimize flake without bloating happy-path runtime.
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

    /// Standard initialize + notifications/initialized handshake.
    fn handshake(&mut self) -> Value {
        let resp = self.send_and_recv(
            "initialize",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0.1" }
                }
            }),
        );
        self.send_notification(&json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }));
        resp
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

/// Spawn with credentials scrubbed — used for the no-creds smoke test.
fn spawn_no_creds(stderr_dir: &tempfile::TempDir) -> StdioMcpHarness {
    StdioMcpHarness::spawn(
        stderr_dir,
        Box::new(|cmd| {
            cmd.env_remove("KODA_EMAIL_IMAP_HOST")
                .env_remove("KODA_EMAIL_USERNAME")
                .env_remove("KODA_EMAIL_PASSWORD");
        }),
    )
}

/// Spawn with credentials inherited from the environment — for live tests.
/// Returns None (and the test should early-return) if creds aren't set.
fn spawn_with_creds(stderr_dir: &tempfile::TempDir) -> Option<StdioMcpHarness> {
    if std::env::var("KODA_EMAIL_IMAP_HOST").is_err()
        || std::env::var("KODA_EMAIL_USERNAME").is_err()
        || std::env::var("KODA_EMAIL_PASSWORD").is_err()
    {
        eprintln!("Skipping: KODA_EMAIL_* env vars not set");
        return None;
    }
    Some(StdioMcpHarness::spawn(stderr_dir, Box::new(|_| {})))
}

// ── Layer 1: MCP Protocol Smoke Test (always run) ────────────

/// One consolidated smoke test of the MCP protocol surface without
/// credentials. Walks through:
///   1. `initialize` returns serverInfo with name=koda-email,
///   2. `tools/list` exposes EmailRead, EmailSend, EmailSearch with schemas,
///   3. Each tool returns a graceful error mentioning KODA_EMAIL when called
///      without credentials, instead of crashing.
///
/// Wrapped in a one-shot retry: rmcp's tokio-based stdio transport has a
/// rare race on macOS pipe wake-ups (~3 % flake locally) where a queued
/// response never reaches the OS pipe. The test is fully idempotent (fresh
/// subprocess per attempt), so retrying once collapses effective flake to
/// ~0.1 % — well below CI noise. If both attempts fail, the *second*
/// panic surfaces with full diagnostics so we still see what broke.
#[test]
fn test_mcp_protocol_smoke_no_creds() {
    run_with_retry("mcp_protocol_smoke_no_creds", run_no_creds_smoke);
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

fn run_no_creds_smoke() {
    let stderr_dir = tempfile::TempDir::new().unwrap();
    let mut srv = spawn_no_creds(&stderr_dir);

    // 1. handshake
    let init = srv.handshake();
    assert_eq!(
        init["result"]["serverInfo"]["name"], "koda-email",
        "initialize: serverInfo.name should be 'koda-email'"
    );
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "initialize: capabilities.tools should be an object"
    );

    // 2. tools/list — verify presence and schema completeness
    let resp = srv.send_and_recv(
        "tools/list",
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    );
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools/list: result.tools should be an array");
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for required in ["EmailRead", "EmailSend", "EmailSearch"] {
        assert!(
            tool_names.contains(&required),
            "tools/list: should include {required}, got: {tool_names:?}"
        );
    }
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

    // 3. graceful degradation — each tool errors with KODA_EMAIL guidance
    let cases = [
        (
            3,
            "EmailRead",
            json!({ "count": 5 }),
            "EmailRead without creds",
        ),
        (
            4,
            "EmailSend",
            json!({ "to": "test@example.com", "subject": "Test", "body": "Hello" }),
            "EmailSend without creds",
        ),
        (
            5,
            "EmailSearch",
            json!({ "query": "test" }),
            "EmailSearch without creds",
        ),
    ];
    for (id, tool_name, args, label) in cases {
        let resp = srv.send_and_recv(
            label,
            &json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": tool_name, "arguments": args }
            }),
        );
        let error = &resp["error"];
        assert!(
            error.is_object(),
            "[{label}] should return JSON-RPC error: {resp}"
        );
        let message = error["message"].as_str().unwrap_or("");
        assert!(
            message.contains("KODA_EMAIL"),
            "[{label}] error should mention KODA_EMAIL env vars: {message}"
        );
    }

    srv.shutdown();
}

/// `--version` is a separate code path that doesn't go through the MCP
/// stdio server; keep it as its own one-shot test.
#[test]
fn test_version_flag() {
    let binary = koda_email_binary();
    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("Failed to run koda-email --version");
    assert!(output.status.success(), "--version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("koda-email"),
        "--version should print 'koda-email': {stdout}"
    );
}

// ── Layer 2: Live Email Tests (opt-in, #[ignore]) ───────────
//
// Each lives as its own `#[ignore]` test so they can be invoked
// individually from CI/dev. They share the harness via `spawn_with_creds`.

#[test]
#[ignore]
fn test_live_email_read() {
    let stderr_dir = tempfile::TempDir::new().unwrap();
    let Some(mut srv) = spawn_with_creds(&stderr_dir) else {
        return;
    };
    srv.handshake();

    let resp = srv.send_and_recv(
        "live EmailRead",
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailRead",
                "arguments": { "count": 3 }
            }
        }),
    );
    assert!(
        resp["error"].is_null(),
        "EmailRead should succeed with valid creds: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "Should return some output (emails or 'no emails')"
    );
    eprintln!("EmailRead result:\n{text}");

    srv.shutdown();
}

#[test]
#[ignore]
fn test_live_email_search() {
    let stderr_dir = tempfile::TempDir::new().unwrap();
    let Some(mut srv) = spawn_with_creds(&stderr_dir) else {
        return;
    };
    srv.handshake();

    let resp = srv.send_and_recv(
        "live EmailSearch",
        &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "EmailSearch",
                "arguments": { "query": "test", "max_results": 3 }
            }
        }),
    );
    assert!(
        resp["error"].is_null(),
        "EmailSearch should succeed: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !text.is_empty(),
        "Should return search results or 'no results'"
    );
    eprintln!("EmailSearch result:\n{text}");

    srv.shutdown();
}
