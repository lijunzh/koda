//! `--mcp-config` adds MCP servers for one run and saves nothing.
//!
//! Runs the `koda` binary headless against a local model stub that calls
//! the MCP tool once, with a `sh` stdio server given only through the flag.
//! No API key, no model.

#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use koda_test_utils::openai::sse;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
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

/// The config file `run_echo_turn` runs with.
fn config_file(tmp: &tempfile::TempDir) -> koda_config::ConfigFile {
    koda_config::ConfigFile::in_dir(&tmp.path().join("koda"))
}

/// Run one headless turn whose model calls `mail__echo` once. `flag` is
/// passed as `--mcp-config` when set; `stored` is written to config.toml as
/// `[mcp_servers.mail]` first. Returns the chat request bodies and the
/// config dir.
async fn run_echo_turn(
    flag: Option<String>,
    stored: Option<serde_json::Value>,
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
    if let Some(server) = stored {
        let edit = koda_config::Edit {
            key_path: "mcp_servers.mail".into(),
            value: server,
        };
        config_file(&tmp).edit(None, &[edit]).unwrap();
    }
    let mut cmd = tokio::process::Command::new(koda_bin());
    cmd.args(["exec", "Echo something"])
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

    let stored = config_file(&tmp).load().unwrap().config.mcp_servers;
    assert!(stored.is_empty(), "saved: {stored:?}");
}

/// A server in config.toml connects after the agent is built; its tools
/// must still reach the model.
#[tokio::test(flavor = "multi_thread")]
async fn a_configured_server_is_offered_to_the_model() {
    let stored = json!({
        "command": "sh",
        "args": ["-c", ECHO_SERVER],
    });
    let (bodies, _tmp) = run_echo_turn(None, Some(stored)).await;
    assert_echo_round_trip(&bodies);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_flag_stops_the_start() {
    let tmp = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(koda_bin())
        .args(["exec", "hi", "--provider", "lmstudio", "--model", "stub"])
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

/// A server that can't start is named on stderr with the reason, as
/// `codex exec` warns: the run goes on without its tools. Its URL's key
/// stays out of it.
#[tokio::test(flavor = "multi_thread")]
async fn exec_says_why_a_server_did_not_start() {
    let server = FakeLlmServer::spawn().await;
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| final_text())
        .await;
    let tmp = tempfile::tempdir().unwrap();
    // Koda refuses a loopback server, so this one fails to start.
    let edit = koda_config::Edit {
        key_path: "mcp_servers.inside".into(),
        value: json!({"url": "http://127.0.0.1:9/mcp?apiKey=TEST_SECRET"}),
    };
    config_file(&tmp).edit(None, &[edit]).unwrap();
    let run = |args: &'static [&'static str]| {
        let mut cmd = tokio::process::Command::new(koda_bin());
        cmd.args(["exec", "hi"])
            .args(args)
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .arg("--project-root")
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .kill_on_drop(true);
        async move {
            let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
                .await
                .expect("koda hung")
                .expect("failed to run koda");
            assert!(output.status.success(), "{output:?}");
            (
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            )
        }
    };
    let (stdout, stderr) = run(&[]).await;
    assert_eq!(
        stderr.matches("MCP server 'inside' unavailable: ").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains("loopback") || stderr.contains("private"),
        "{stderr}"
    );
    assert!(!stderr.contains("TEST_SECRET"), "{stderr}");
    assert!(stdout.contains("Done."), "{stdout}");

    // `--json` has the notification, and no extra text.
    let (stdout, stderr) = run(&["--json"]).await;
    assert!(
        stdout.contains("mcpServer/startupStatus/updated") && stdout.contains("\"failed\""),
        "{stdout}"
    );
    assert!(!stdout.contains("TEST_SECRET"), "{stdout}");
    assert!(!stderr.contains("unavailable"), "{stderr}");
}
