//! `koda exec --mode plan` is a read-only run: tools that read run, Bash runs
//! in a read-only sandbox, and everything else is refused without asking
//! anyone.
//!
//! Runs the `koda` binary headless against a local model stub whose first
//! reply calls a file write, a shell write and two MCP tools, one of them
//! marked read-only by its server. No API key, no model.

#![cfg(unix)]

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use koda_test_utils::openai::sse;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
}

/// A stdio MCP server with two tools: `lookup`, which its server marks
/// read-only, and `send`, which it doesn't.
const MAIL_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mail","version":"0.1.0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"lookup","description":"Look a message up","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}},{"name":"send","description":"Send a message","inputSchema":{"type":"object","properties":{}}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*'"name":"lookup"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"lookup-ran"}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"send-ran"}]}}\n' "$id" ;;
  esac
done
"#;

fn call(index: usize, name: &str, args: Value) -> Value {
    json!({
        "index": index,
        "id": format!("call_{index}"),
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    })
}

fn tool_calls() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [
            call(0, "Write", json!({"file_path": "written.txt", "content": "x"})),
            call(1, "Bash", json!({"command": "touch touched.txt"})),
            call(2, "mail__lookup", json!({})),
            call(3, "mail__send", json!({})),
        ]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn final_text() -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"content": "Done."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_runs_reads_and_refuses_the_rest() {
    let server = FakeLlmServer::spawn().await;
    let turns = AtomicUsize::new(0);
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_calls()
            } else {
                final_text()
            }
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let mcp = json!({"mcpServers": {"mail": {
        "command": "sh",
        "args": ["-c", MAIL_SERVER],
        "startupTimeoutSec": 20,
    }}})
    .to_string();
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(koda_bin())
            .args(["exec", "Check my mail"])
            .args(["--mode", "plan"])
            .args(["--provider", "lmstudio", "--model", "stub"])
            .args(["--base-url", &server.url()])
            .args(["--mcp-config", &mcp])
            .arg("--project-root")
            .arg(&project)
            .env("XDG_CONFIG_HOME", tmp.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("koda hung")
    .expect("failed to run koda");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");

    assert!(!project.join("written.txt").exists(), "Write ran");
    assert!(!project.join("touched.txt").exists(), "touch ran");

    let bodies: Vec<String> = server
        .received_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert!(bodies.len() >= 2, "{} model call(s)", bodies.len());
    assert!(
        bodies[0].contains("Read-only session"),
        "system prompt doesn't say the run is read-only: {}",
        bodies[0]
    );
    let after = &bodies[1];
    assert!(
        after.contains("lookup-ran"),
        "read-only MCP tool didn't run: {after}"
    );
    assert!(!after.contains("send-ran"), "unmarked MCP tool ran");
    // Bash ran, and the read-only sandbox stopped its write.
    assert!(
        after.contains("touch: touched.txt: Operation not permitted"),
        "Bash wasn't stopped at the kernel: {after}"
    );
    // Write and mail__send each come back refused.
    assert_eq!(
        after.matches("[plan mode] Not run").count(),
        2,
        "expected two refusals: {after}"
    );
}
