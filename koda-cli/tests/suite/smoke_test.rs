//! Smoke tests: headless mode against a scripted stub server.
//!
//! These tests exercise the full binary pipeline without a real LLM.
//! They run `koda exec "..."`, as text or with `--json`, against a
//! [`ScriptedServer`] playing scripted responses.
//!
//! CI-safe: no network, no API keys, no LLM required.

use koda_test_utils::network::ScriptedServer;
use std::process::Command;

// ── Helpers ─────────────────────────────────────────────────

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
}

/// `koda exec --json` against a scripted stub server.
fn run_mock(prompt: &str, responses: &str) -> (String, String, bool) {
    run_mock_exec_as(&["--json"], prompt, responses, None)
}

/// `koda exec` as text: the reply on stdout, progress on stderr.
fn run_mock_with_mode(prompt: &str, responses: &str, mode: Option<&str>) -> (String, String, bool) {
    run_mock_exec_as(&[], prompt, responses, mode)
}

fn run_mock_exec_as(
    flags: &[&str],
    prompt: &str,
    responses: &str,
    mode: Option<&str>,
) -> (String, String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let server = ScriptedServer::start(responses);
    let mut cmd = Command::new(koda_bin());
    cmd.arg("exec")
        .args(flags)
        .args([prompt, "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(server.koda_args());
    if let Some(m) = mode {
        cmd.args(["--mode", m]);
    }
    let output = cmd.output().expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

/// The `exec/summary` that ends `koda exec --json`'s stdout.
fn extract_json(stdout: &str) -> serde_json::Value {
    let last = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("No output on stdout:\n{stdout}"));
    let message: serde_json::Value =
        serde_json::from_str(last).unwrap_or_else(|e| panic!("Invalid JSON: {e}\nfrom: {last}"));
    assert_eq!(message["method"], "exec/summary", "{stdout}");
    message["params"].clone()
}
// ── Headless tests on a scripted server ──────────────────────────────

// ── Headless safety: destructive commands always rejected at top-level (#1250) ──

#[test]
fn mock_destructive_bash_rejected_in_auto_mode_in_headless() {
    // #1250: Destructive operations require human confirmation in EVERY trust
    // mode, including Auto. The user said YOLO for normal work, not for
    // `rm -rf /`. In headless mode, no human is available to confirm, so
    // destructive operations are auto-rejected by policy at all trust levels.
    //
    // Pre-#1250 this test asserted the opposite: Auto + destructive auto-
    // approved within the sandbox boundary. That hole is closed: the trust
    // matrix now returns NeedsConfirmation for Auto × Destructive, which the
    // headless policy then auto-rejects. Trust layer + headless policy agree.
    let responses = r#"[
        {"tool":"Bash","args":{"command":"rm -rf /tmp/nonexistent_test_dir"}},
        {"text":"Done."}
    ]"#;

    let (_stdout, stderr, _success) =
        run_mock_with_mode("delete everything", responses, Some("auto"));
    assert!(
        stderr.contains("Rejected destructive action"),
        "#1250: Auto mode in headless MUST still reject destructive Bash. \
         The user gave YOLO consent for ordinary work, not for rm -rf.\n\
         stderr: {stderr}"
    );
}

#[test]
fn mock_destructive_bash_rejected_in_default_mode_in_headless() {
    // End-to-end regression test for #1241: with no `--mode`, headless runs
    // in the default (`Auto`), which must still reject destructive Bash; the
    // user gave consent for ordinary work, not for rm -rf.
    let responses = r#"[
        {"tool":"Bash","args":{"command":"rm -rf /tmp/nonexistent_test_dir"}},
        {"text":"Done."}
    ]"#;

    let (_stdout, stderr, _success) = run_mock_with_mode("delete everything", responses, None);
    assert!(
        stderr.contains("Rejected destructive action"),
        "The default Auto mode MUST reject destructive Bash. \
         If this fails, #1241 has regressed.\nstderr: {stderr}"
    );
}

/// `koda exec`'s text names a sub-agent and shows its tool calls indented
/// under its `InvokeAgent` call. The parent and the sub-agent share the
/// script: the parent delegates, then the sub-agent reads and answers.
#[test]
fn exec_text_shows_a_sub_agents_calls_under_its_call() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "mock test content").unwrap();
    let responses = r#"[
        {"tool":"InvokeAgent","args":{"agent_name":"explore","prompt":"read hello.txt"}},
        {"tool":"Read","args":{"path":"hello.txt"}},
        {"text":"Found it."}
    ]"#;
    let output = Command::new(koda_bin())
        .args(["exec", "ask a sub-agent", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(responses).koda_args())
        .output()
        .expect("Failed to run koda");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(stderr.contains("  \u{26a1} InvokeAgent"), "{stderr}");
    assert!(stderr.contains("\u{1f916} explore"), "{stderr}");
    assert!(stderr.contains("\n\x1b[36m    \u{26a1} Read"), "{stderr}");
    assert!(!stderr.contains("[bg task"), "{stderr}");
}

/// A write that plan mode blocks shows what it would have done, with the
/// new file's lines.
#[test]
fn exec_text_shows_a_blocked_write_and_its_content() {
    let responses = r#"[
        {"tool":"Write","args":{"path":"new.txt","content":"fresh line\n"}},
        {"text":"Planned."}
    ]"#;
    let (stdout, stderr, success) = run_mock_with_mode("write a file", responses, Some("plan"));
    assert!(success, "stderr: {stderr}");
    assert!(stderr.contains("Would execute"), "{stderr}");
    assert!(stderr.contains("+ new.txt (new file)"), "{stderr}");
    assert!(stderr.contains("+fresh line"), "{stderr}");
    assert!(stdout.contains("Planned."), "{stdout}");
}

/// Run a session with a prompt big enough that the engine compacts mid-turn,
/// as soon as there is history to compact (after the second tool call), then
/// finishes with `final_response`.
fn run_mock_compacted_session(final_response: &str) -> (serde_json::Value, String) {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(tmp.path().join(name), "contents\n").unwrap();
    }
    let responses = serde_json::json!([
        {"tool": "Read", "args": {"file_path": "a.txt"}},
        {"tool": "Read", "args": {"file_path": "b.txt"}},
        {"text": "<summary>the reads so far</summary>"},
        {"text": final_response},
    ]);
    // ~11K of the 16K window: past the 85% trigger once the system prompt is
    // counted, and still small enough to summarize.
    let prompt = format!("read both files {}", "padding ".repeat(4_700));
    let output = Command::new(koda_bin())
        .args(["exec", "--json", prompt.as_str(), "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(&responses.to_string()).koda_args())
        .args(["--max-context-tokens", "16000"])
        .output()
        .expect("Failed to run koda");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stdout.contains(r#""type":"contextCompaction""#),
        "no compaction ran: {stdout}\nstderr: {stderr}"
    );
    (extract_json(&stdout), stderr)
}

/// Regression for #1444: the compaction hint was an assistant row, so it
/// could come back as the run's `response`.
#[test]
fn mock_compaction_hint_is_never_the_response() {
    let (json, stderr) = run_mock_compacted_session("The final answer.");
    assert_eq!(json["response"], "The final answer.", "stderr: {stderr}");

    // The case that was seen: the model ends on nothing after tool use.
    let (json, stderr) = run_mock_compacted_session("");
    assert_eq!(json["response"], "", "stderr: {stderr}");
}

/// `koda exec` against a scripted server: read hello.txt, then reply.
fn run_mock_exec(flags: &[&str]) -> (String, String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "mock test content").unwrap();
    let responses = r#"[
        {"tool":"Read","args":{"path":"hello.txt"}},
        {"text":"I read the file."}
    ]"#;
    let output = Command::new(koda_bin())
        .arg("exec")
        .args(flags)
        .args(["read hello.txt", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(responses).koda_args())
        .output()
        .expect("Failed to run koda");
    (
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.success(),
    )
}

/// Every stdout line as JSON, and its method (with the item type for
/// `item/*`).
fn protocol_lines(stdout: &str) -> (Vec<serde_json::Value>, Vec<String>) {
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("non-JSON stdout line ({e}): {line:?}"))
        })
        .collect();
    let methods = lines
        .iter()
        .map(|v| {
            assert_eq!(v["jsonrpc"], "2.0", "{v}");
            let method = v["method"]
                .as_str()
                .unwrap_or_else(|| panic!("no method: {v}"));
            match v["params"]["item"]["type"].as_str() {
                Some(kind) => format!("{method} {kind}"),
                None => method.to_string(),
            }
        })
        .collect();
    (lines, methods)
}

#[test]
fn exec_json_prints_the_protocol_from_session_to_summary() {
    let (stdout, stderr, success) = run_mock_exec(&["--json"]);
    assert!(success, "Failed.\nstderr: {stderr}");
    let (lines, methods) = protocol_lines(&stdout);
    assert_eq!(
        methods,
        [
            "session/started",
            // The stub reports no context window, so Koda says what it assumed.
            "warning",
            "turn/started",
            "item/started userMessage",
            "item/completed userMessage",
            // After each model call, as Codex sends its token count.
            "session/tokenUsage/updated",
            "item/started toolCall",
            "item/completed toolCall",
            "item/started agentMessage",
            "item/completed agentMessage",
            "session/tokenUsage/updated",
            "turn/completed",
            "exec/summary",
        ],
        "{stdout}"
    );
    let started = &lines[0]["params"];
    assert_eq!(started["kodaVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(started["session"]["modelProvider"], "lm-studio");
    let prompt = &lines[4]["params"]["item"];
    assert_eq!(
        prompt["content"],
        serde_json::json!([{"type": "text", "text": "read hello.txt"}])
    );
    let usage = &lines[5]["params"]["tokenUsage"];
    assert!(
        usage["last"]["totalTokens"].as_u64().unwrap() > 0,
        "{usage}"
    );
    assert!(usage["modelContextWindow"].as_u64().unwrap() > 0, "{usage}");
    let read = &lines[7]["params"]["item"];
    assert_eq!(read["tool"], "Read");
    assert_eq!(read["status"], "completed");
    assert!(
        read["output"]
            .as_str()
            .unwrap()
            .contains("mock test content")
    );
    assert_eq!(lines[9]["params"]["item"]["text"], "I read the file.");
    assert_eq!(lines[11]["params"]["turn"]["status"], "completed");
    let summary = &lines[12]["params"];
    assert_eq!(summary["success"], true);
    assert_eq!(summary["toolCalls"]["Read"], 1);
    assert_eq!(summary["turnStatus"], "completed");
    assert_eq!(summary["response"], "I read the file.");
}

#[test]
fn exec_json_deltas_adds_the_streaming_text() {
    let (stdout, stderr, success) = run_mock_exec(&["--json", "--json-deltas"]);
    assert!(success, "Failed.\nstderr: {stderr}");
    let (lines, methods) = protocol_lines(&stdout);
    let text: String = lines
        .iter()
        .filter(|v| v["method"] == "item/agentMessage/delta")
        .map(|v| v["params"]["delta"].as_str().unwrap())
        .collect();
    assert_eq!(text, "I read the file.", "{methods:?}");
}

#[test]
fn exec_writes_the_last_reply_to_a_file() {
    let out = tempfile::tempdir().unwrap();
    let path = out.path().join("last.txt");
    let (stdout, stderr, success) = run_mock_exec(&["-o", path.to_str().unwrap()]);
    assert!(success, "Failed.\nstderr: {stderr}");
    assert!(stdout.contains("I read the file."), "{stdout}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "I read the file.");
}

/// `koda -p` and `--output-format` (`stream-json` included) were removed in
/// 0.7.0: each stops before any run and names `koda exec`. (`koda "prompt"`
/// opens the TUI with it; `tui_pty_test` covers that.)
#[test]
fn removed_one_shot_forms_are_refused_with_their_replacement() {
    for args in [
        &["-p", "say hi"][..],
        &["--prompt", "say hi"],
        &["-p", "-"],
        &["--output-format", "json"],
        &["--output-format", "stream-json"],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let output = Command::new(koda_bin())
            .args(args)
            .args(["--project-root"])
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .output()
            .expect("Failed to run koda");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains("koda exec"), "{args:?}: {stderr}");
    }
}

/// `koda` alone opens the TUI, which needs a terminal. A piped prompt is
/// `koda exec`'s, as `codex` refuses one too.
#[test]
fn a_piped_prompt_without_exec_is_refused() {
    use std::io::Write as _;
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(koda_bin())
        .args(["--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to run koda");
    // Koda may exit before reading it; a broken pipe is fine.
    let _ = child.stdin.take().unwrap().write_all(b"say hi");
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("koda exec"), "{stderr}");
}

#[test]
fn mock_model_name_in_json() {
    let (stdout, _, _) = run_mock("say hi", r#"[{"text":"ok"}]"#);
    let json = extract_json(&stdout);
    let model = json["model"].as_str().unwrap_or("");
    // Mock provider reports "mock-model" but config may load "auto-detect" first
    assert!(!model.is_empty(), "Expected model name in JSON, got empty");
}

#[test]
fn mock_at_file_reference() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("data.txt"), "important data").unwrap();

    let output = Command::new(koda_bin())
        .args(["exec", "--json", "analyze @data.txt", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(r#"[{"text":"analyzed"}]"#).koda_args())
        .output()
        .expect("Failed to run koda");

    assert!(
        output.status.success(),
        "@file processing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The mention reaches the host as a path, with the text as typed.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let user = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|line| {
            line["method"] == "item/completed" && line["params"]["item"]["type"] == "userMessage"
        })
        .expect("no userMessage item");
    // The text, then the file it mentioned, as Codex's `content`.
    assert_eq!(
        user["params"]["item"]["content"],
        serde_json::json!([
            {"type": "text", "text": "analyze @data.txt"},
            {"type": "mention", "name": "data.txt", "path": "data.txt"},
        ])
    );
}

/// Regression: --resume flag must work as an alias for --session (#505/#507).
#[test]
fn mock_session_resume_via_resume_flag() {
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1: create session
    let output1 = Command::new(koda_bin())
        .args(["exec", "--json", "turn one", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(r#"[{"text":"first"}]"#).koda_args())
        .output()
        .expect("Turn 1 failed");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let json1 = extract_json(&stdout1);
    let session_id = json1["sessionId"].as_str().expect("No session_id");

    // Turn 2: resume via --resume (not --session)
    let output2 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn two",
            "--resume",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(ScriptedServer::start(r#"[{"text":"second via resume"}]"#).koda_args())
        .output()
        .expect("Turn 2 with --resume failed");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(
        output2.status.success(),
        "--resume should work as --session alias.\nstderr: {stderr2}"
    );
    let json2 = extract_json(&stdout2);
    assert_eq!(json2["success"], true);
    assert_eq!(
        json2["sessionId"].as_str().unwrap(),
        session_id,
        "--resume should resume the same session"
    );
}
