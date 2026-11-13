//! Smoke tests: headless mode with MockProvider.
//!
//! These tests exercise the full binary pipeline without a real LLM.
//! They run `koda exec "..." --provider mock`, as text or with `--json`,
//! with scripted responses via KODA_MOCK_RESPONSES env var.
//!
//! CI-safe: no network, no API keys, no LLM required.

use std::process::Command;

// ── Helpers ─────────────────────────────────────────────────

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

/// `koda exec --json` on the mock provider.
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
    let mut cmd = Command::new(koda_bin());
    cmd.arg("exec")
        .args(flags)
        .args([prompt, "--provider", "mock", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses);
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
// ── Headless MockProvider tests ──────────────────────────────

#[test]
fn mock_text_response_returns_json() {
    let (stdout, stderr, success) = run_mock("say hi", r#"[{"text":"Hello from mock!"}]"#);
    assert!(success, "Process failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("Hello from mock"),
        "Expected 'Hello from mock' in response, got: {response}"
    );
}

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
fn mock_destructive_bash_rejected_in_safe_and_default_modes_in_headless() {
    // End-to-end regression test for #982 + #1241. Pre-fix, headless hardcoded
    // `TrustMode::Auto`, so `--mode safe` was silently ignored and destructive
    // actions ran without confirmation. Post-fix, `--mode safe` is honored.
    //
    // We test BOTH explicit `--mode safe` and the current default (`Auto` since
    // #1241). Both must reject destructive Bash; the user gave consent for
    // ordinary work, not for rm -rf. Tiny detail, huge difference, no bananas.
    let responses = r#"[
        {"tool":"Bash","args":{"command":"rm -rf /tmp/nonexistent_test_dir"}},
        {"text":"Done."}
    ]"#;

    for mode in [Some("safe"), None] {
        let label = mode.unwrap_or("<default>");
        let (_stdout, stderr, _success) = run_mock_with_mode("delete everything", responses, mode);
        assert!(
            stderr.contains("Rejected destructive action"),
            "[mode={label}] Safe and default Auto MUST reject destructive Bash. \
             If this fails, #982/#1241 has regressed.\nstderr: {stderr}"
        );
    }
}

#[test]
fn mock_empty_responses_succeeds() {
    let (stdout, stderr, success) = run_mock("say hi", "[]");
    assert!(success, "Process failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
}

#[test]
fn mock_tool_use_read_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "mock test content").unwrap();

    let responses = r#"[
        {"tool":"Read","args":{"path":"hello.txt"}},
        {"text":"I read the file."}
    ]"#;

    let output = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "read hello.txt",
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses)
        .output()
        .expect("Failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "Failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        response.contains("read the file"),
        "Expected tool result in response, got: {response}\nstderr: {stderr}"
    );
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
        .args([
            "exec",
            "--json",
            prompt.as_str(),
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses.to_string())
        .env("KODA_MAX_CONTEXT_TOKENS", "16000")
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

/// `koda exec` on the mock provider: read hello.txt, then reply.
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
        .args(["read hello.txt", "--provider", "mock", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses)
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
            "turn/started",
            "item/started toolCall",
            "item/completed toolCall",
            "item/started agentMessage",
            "item/completed agentMessage",
            "turn/completed",
            "exec/summary",
        ],
        "{stdout}"
    );
    let started = &lines[0]["params"];
    assert_eq!(started["protocolVersion"], 2);
    assert_eq!(started["session"]["provider"], "mock");
    let read = &lines[3]["params"]["item"];
    assert_eq!(read["tool"], "Read");
    assert_eq!(read["status"], "completed");
    assert!(
        read["output"]
            .as_str()
            .unwrap()
            .contains("mock test content")
    );
    assert_eq!(lines[5]["params"]["item"]["text"], "I read the file.");
    assert_eq!(lines[6]["params"]["turn"]["status"], "completed");
    let summary = &lines[7]["params"];
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

/// `koda -p`, `--output-format` (`stream-json` included) and a bare prompt
/// were removed in 0.7.0: each stops before any run and names `koda exec`.
#[test]
fn removed_one_shot_forms_are_refused_with_their_replacement() {
    for args in [
        &["-p", "say hi"][..],
        &["--prompt", "say hi"],
        &["-p", "-"],
        &["say hi"],
        &["--output-format", "json"],
        &["--output-format", "stream-json"],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let output = Command::new(koda_bin())
            .args(args)
            .args(["--provider", "mock", "--project-root"])
            .arg(tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .env("KODA_MOCK_RESPONSES", r#"[{"text":"ran"}]"#)
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
        .args(["--provider", "mock", "--project-root"])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"ran"}]"#)
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
fn mock_error_response_handled() {
    let (stdout, _stderr, _success) = run_mock("say hi", r#"[{"error":"Simulated LLM failure"}]"#);
    let json = extract_json(&stdout);
    // Provider error → success=false or empty response
    let response = json["response"].as_str().unwrap_or("");
    assert!(
        json["success"] == false || response.is_empty(),
        "Expected failure indication in: {json}"
    );
}

#[test]
fn mock_session_id_returned() {
    let (stdout, stderr, _) = run_mock("say hi", r#"[{"text":"ok"}]"#);
    let json = extract_json(&stdout);
    let session_id = json["sessionId"].as_str();
    assert!(
        session_id.is_some() && !session_id.unwrap().is_empty(),
        "Expected session_id in JSON.\nJSON: {json}\nstderr: {stderr}"
    );
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
        .args([
            "exec",
            "--json",
            "analyze @data.txt",
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"analyzed"}]"#)
        .output()
        .expect("Failed to run koda");

    assert!(
        output.status.success(),
        "@file processing failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mock_multi_turn_tool_use() {
    let responses = r#"[
        {"tool":"Bash","args":{"command":"echo hello"}},
        {"text":"Command output was hello."}
    ]"#;
    let (stdout, stderr, success) = run_mock("run echo hello", responses);
    assert!(success, "Multi-turn failed.\nstderr: {stderr}");
    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);
}

#[test]
fn mock_session_resume() {
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1
    let output1 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn one",
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"first"}]"#)
        .output()
        .expect("Turn 1 failed");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let json1 = extract_json(&stdout1);
    let session_id = json1["sessionId"].as_str().expect("No session_id");

    // Turn 2: resume
    let output2 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn two",
            "--provider",
            "mock",
            "--session",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"second"}]"#)
        .output()
        .expect("Turn 2 failed");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(
        !stdout2.is_empty(),
        "Turn 2 produced no stdout.\nstderr: {stderr2}"
    );
    let json2 = extract_json(&stdout2);
    assert_eq!(json2["success"], true);
    assert_eq!(
        json2["sessionId"].as_str().unwrap(),
        session_id,
        "Resumed session should keep same ID"
    );
}

/// Regression: --resume flag must work as an alias for --session (#505/#507).
#[test]
fn mock_session_resume_via_resume_flag() {
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1: create session
    let output1 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn one",
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"first"}]"#)
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
            "--provider",
            "mock",
            "--resume",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"second via resume"}]"#)
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

/// Regression: -s short flag must work for --resume (#505).
#[test]
fn mock_session_resume_via_short_s_flag() {
    let tmp = tempfile::tempdir().unwrap();

    // Turn 1
    let output1 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn one",
            "--provider",
            "mock",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"first"}]"#)
        .output()
        .expect("Turn 1 failed");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let json1 = extract_json(&stdout1);
    let session_id = json1["sessionId"].as_str().expect("No session_id");

    // Turn 2: resume via -s
    let output2 = Command::new(koda_bin())
        .args([
            "exec",
            "--json",
            "turn two",
            "--provider",
            "mock",
            "-s",
            session_id,
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", r#"[{"text":"second via -s"}]"#)
        .output()
        .expect("Turn 2 with -s failed");

    assert!(
        output2.status.success(),
        "-s should work as --resume alias.\nstderr: {}",
        String::from_utf8_lossy(&output2.stderr)
    );
    let json2 = extract_json(&String::from_utf8_lossy(&output2.stdout));
    assert_eq!(
        json2["sessionId"].as_str().unwrap(),
        session_id,
        "-s should resume the same session"
    );
}
