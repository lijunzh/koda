//! `--mcp-config` adds MCP servers for one run and saves nothing.
//!
//! Runs the `koda` binary headless against a local model stub that calls
//! the MCP tool once, with a `sh` stdio server given only through the flag.
//! No API key, no model.

#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// A minimal stdio MCP server with one tool, `echo`.
const ECHO_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"echo","version":"0.1.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the message back","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      message=$(printf '%s\n' "$line" | sed -n 's/.*"message":"\([^"]*\)".*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echoed %s"}]}}\n' "$id" "$message" ;;
  esac
done
"#;

fn echo_call() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "mail__echo",
                "arguments": json!({"message": "flag-only-server"}).to_string(),
            },
        }]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": "Done."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

/// Run one headless turn whose model calls `mail__echo` once. `flag` is
/// passed as `--mcp-config` when set; `stored` is saved to the database first.
/// Returns the chat request bodies and the config dir.
async fn run_echo_turn(
    flag: Option<String>,
    stored: Option<koda_core::mcp::McpServerConfig>,
) -> (Vec<String>, tempfile::TempDir) {
    let server = FakeLlmServer::spawn().await;
    let turns = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                echo_call()
            } else {
                final_text()
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    if let Some(config) = stored {
        let db = koda_core::db::Database::init(&tmp.path().join("koda"))
            .await
            .unwrap();
        koda_core::mcp::config::save_mcp_config(&db, "mail", &config)
            .await
            .unwrap();
    }
    let mut cmd = tokio::process::Command::new(koda_bin());
    cmd.args(["-p", "Echo something", "--output-format", "json"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true);
    if let Some(flag) = flag {
        cmd.args(["--mcp-config", &flag]);
    }
    let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bodies = server
        .received_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    (bodies, tmp)
}

/// The model was offered `mail__echo`, called it, and got the server's
/// answer back.
fn assert_echo_round_trip(bodies: &[String]) {
    assert!(bodies.len() >= 2, "{} model call(s)", bodies.len());
    assert!(
        bodies[0].contains("mail__echo"),
        "tool not offered: {}",
        bodies[0]
    );
    assert!(
        bodies[1].contains("echoed flag-only-server"),
        "no tool result from the server: {}",
        bodies[1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_server_answers_tool_calls_and_is_not_saved() {
    let flag = json!({"mcpServers": {"mail": {
        "command": "sh",
        "args": ["-c", ECHO_SERVER],
        "startupTimeoutSec": 20,
    }}})
    .to_string();
    let (bodies, tmp) = run_echo_turn(Some(flag), None).await;
    assert_echo_round_trip(&bodies);

    let db = koda_core::db::Database::init(&tmp.path().join("koda"))
        .await
        .unwrap();
    let stored = koda_core::mcp::config::list_mcp_server_names(&db)
        .await
        .unwrap();
    assert!(stored.is_empty(), "saved: {stored:?}");
}

/// A server stored with `/mcp add` connects after the agent is built; its
/// tools must still reach the model.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_server_is_offered_to_the_model() {
    let stored: koda_core::mcp::McpServerConfig = serde_json::from_value(json!({
        "transport": "stdio",
        "command": "sh",
        "args": ["-c", ECHO_SERVER],
    }))
    .unwrap();
    let (bodies, _tmp) = run_echo_turn(None, Some(stored)).await;
    assert_echo_round_trip(&bodies);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_flag_stops_the_start() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(koda_bin())
        .args(["-p", "hi", "--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", "http://127.0.0.1:9"])
        .args([
            "--mcp-config",
            r#"{"mcpServers": {"old": {"type": "sse", "url": "https://example.com/sse"}}}"#,
        ])
        .arg("--project-root")
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SSE"), "stderr: {stderr}");
}
