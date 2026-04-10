//! E2E tests for built-in skill injection and sub-agent delegation.
//!
//! These tests verify the full pipeline without a real LLM, using the
//! MockProvider and scripted KODA_MOCK_RESPONSES.
//!
//! ## What's tested
//!
//! **koda_docs skill** (injected by koda-cli from compiled docs):
//! - `koda_docs` appears in `ListSkills` output.
//! - `ActivateSkill { skill_name: "koda_docs" }` returns manual content.
//!
//! **Built-in sub-agents** (explore / plan / verify / task):
//! - `InvokeAgent { agent_name: X }` is dispatched and the agent runs.
//! - The `SubAgentStart` event (prints the agent name to stderr) is emitted.
//! - `ListAgents` exposes all four built-in agents.
//!
//! ## Mock provider behaviour
//!
//! Each `create_provider()` call returns a fresh `MockProvider::from_env()`,
//! so sub-agents receive an independent copy of `KODA_MOCK_RESPONSES`.
//! `explore`, `plan`, and `verify` all have `InvokeAgent` in their
//! `disallowed_tools`, so if the mock tries to call it inside those agents
//! the tool dispatch returns an error and the loop advances to the next
//! scripted response.

use std::process::Command;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn koda_bin() -> String {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // test binary name
    path.pop(); // deps/
    path.push("koda");
    path.to_string_lossy().to_string()
}

fn run_mock(prompt: &str, responses: &str) -> (String, String, bool) {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(koda_bin())
        .args([
            "-p",
            prompt,
            "--provider",
            "mock",
            "--output-format",
            "json",
            "--project-root",
        ])
        .arg(tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("KODA_MOCK_RESPONSES", responses)
        .output()
        .expect("failed to run koda");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

fn extract_json(stdout: &str) -> serde_json::Value {
    let start = stdout
        .find('{')
        .unwrap_or_else(|| panic!("no JSON in stdout:\n{stdout}"));
    serde_json::from_str(&stdout[start..])
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nfrom: {}", &stdout[start..]))
}

/// Strip ANSI escape codes so assertions don't need to match colour sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            // consume until a letter (the SGR terminator)
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── koda_docs skill ──────────────────────────────────────────────────────────

/// ListSkills must include `koda_docs` — injected by koda-cli at startup.
#[test]
fn koda_docs_skill_appears_in_list_skills() {
    let responses = r#"[
        {"tool": "ListSkills", "args": {}},
        {"text": "I can see the docs skill."}
    ]"#;
    let (stdout, stderr, success) = run_mock("what skills are available?", responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("koda_docs"),
        "expected 'koda_docs' in ListSkills output.\nstderr: {clean}"
    );
}

/// ActivateSkill for `koda_docs` must return the URL index.
///
/// The tool result (printed to stderr line-by-line) must contain
/// the docs URL — the online manual reference.
#[test]
fn koda_docs_skill_activates_and_returns_manual_content() {
    let responses = r#"[
        {"tool": "ActivateSkill", "args": {"skill_name": "koda_docs"}},
        {"text": "Done reading docs."}
    ]"#;
    let (stdout, stderr, success) = run_mock("how do I use koda?", responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);

    // The skill activation prefix from activate_skill()
    assert!(
        clean.contains("koda_docs"),
        "expected skill name in activation output.\nstderr: {clean}"
    );
    // The URL index pointing to the online manual
    assert!(
        clean.contains("https://lijunzh.github.io/koda/"),
        "expected docs URL in tool result.\nstderr: {clean}"
    );
}

/// Requesting an unknown skill must not crash the process — just surface the
/// "not found" message.  Regression guard: this would fail if koda_docs were
/// somehow double-registered and corrupted the registry.
#[test]
fn unknown_skill_returns_not_found_gracefully() {
    let responses = r#"[
        {"tool": "ActivateSkill", "args": {"skill_name": "no_such_skill_xyz"}},
        {"text": "Skill was not available."}
    ]"#;
    let (_stdout, stderr, success) = run_mock("activate a fake skill", responses);
    // The process should still succeed — the LLM just gets a "not found" tool result
    assert!(
        success,
        "process crashed on missing skill.\nstderr: {stderr}"
    );
    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("not found"),
        "expected 'not found' message in tool result.\nstderr: {clean}"
    );
}

// ── Built-in sub-agent delegation ────────────────────────────────────────────
//
// Pattern for each test:
//   Mock responses: [InvokeAgent to X, "final text"]
//
//   Main agent turn 1  → consumes InvokeAgent response → dispatches sub-agent X.
//   Sub-agent X        → gets a fresh copy of the same mock list; its first
//                        response tries InvokeAgent, which is disallowed for
//                        explore/plan/verify, so the tool dispatch returns an
//                        error; the agent then consumes the "final text"
//                        response and finishes.
//   Main agent turn 2  → consumes "final text" → session completes.
//
//   Assertion: stderr contains the agent name from SubAgentStart.

fn invoke_agent_responses(agent_name: &str, prompt: &str) -> String {
    serde_json::json!([
        {"tool": "InvokeAgent", "args": {"agent_name": agent_name, "prompt": prompt}},
        {"text": format!("{agent_name} delegation done")}
    ])
    .to_string()
}

#[test]
fn explore_sub_agent_is_dispatched_when_invoked() {
    let responses = invoke_agent_responses("explore", "find all Rust source files");
    let (stdout, stderr, success) = run_mock("explore the codebase", &responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("explore"),
        "expected SubAgentStart for 'explore' in stderr.\nstderr: {clean}"
    );
    assert!(
        clean.contains("InvokeAgent"),
        "expected InvokeAgent tool call in stderr.\nstderr: {clean}"
    );
}

#[test]
fn plan_sub_agent_is_dispatched_when_invoked() {
    let responses = invoke_agent_responses("plan", "design a caching layer");
    let (stdout, stderr, success) = run_mock("plan a new feature", &responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("plan"),
        "expected SubAgentStart for 'plan' in stderr.\nstderr: {clean}"
    );
}

#[test]
fn verify_sub_agent_is_dispatched_when_invoked() {
    let responses = invoke_agent_responses("verify", "check the auth module");
    let (stdout, stderr, success) = run_mock("verify my changes", &responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("verify"),
        "expected SubAgentStart for 'verify' in stderr.\nstderr: {clean}"
    );
}

/// Task has InvokeAgent available (it's a full worker), so we use a
/// non-recursive mock: the task sub-agent's scripted Bash response is safe
/// because Bash IS in task's allowed tools and won't cause re-delegation.
#[test]
fn task_sub_agent_is_dispatched_when_invoked() {
    // Main agent: InvokeAgent to task, then final text.
    // Task sub-agent (fresh mock): Bash first (safe), then "task text".
    // We encode both agent scripts in the same env var — task's fresh
    // provider starts at the beginning, so it will first see the
    // InvokeAgent call, which task CAN call.  To avoid infinite recursion,
    // we use a prompt with `agent_name: "explore"` inside the task
    // sub-delegation.  Explore disallows InvokeAgent, so recursion stops.
    let responses = serde_json::json!([
        // Main agent turn 1: delegate to task
        {"tool": "InvokeAgent", "args": {"agent_name": "task", "prompt": "run a bash command"}},
        // Main agent turn 2 (after task returns): final answer
        {"text": "task completed"},
        // task sub-agent turn 1: safe Bash call (no recursion)
        {"tool": "Bash", "args": {"command": "echo task_ran"}},
        // task sub-agent turn 2: finish
        {"text": "task done"}
    ])
    .to_string();

    let (stdout, stderr, success) = run_mock("delegate a task", &responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);
    assert!(
        clean.contains("task"),
        "expected SubAgentStart for 'task' in stderr.\nstderr: {clean}"
    );
}

// ── ListAgents ───────────────────────────────────────────────────────────────

/// ListAgents must expose all four built-in sub-agents.
/// `guide` must NOT appear — it was deleted and replaced by the koda_docs skill.
#[test]
fn list_agents_shows_all_builtin_sub_agents() {
    let responses = r#"[
        {"tool": "ListAgents", "args": {}},
        {"text": "I see the agents."}
    ]"#;
    let (stdout, stderr, success) = run_mock("what agents are available?", responses);
    assert!(success, "process failed.\nstderr: {stderr}");

    let json = extract_json(&stdout);
    assert_eq!(json["success"], true);

    let clean = strip_ansi(&stderr);

    for agent in ["explore", "plan", "verify", "task"] {
        assert!(
            clean.contains(agent),
            "expected built-in agent '{agent}' in ListAgents output.\nstderr: {clean}"
        );
    }
    assert!(
        !clean.contains("guide"),
        "'guide' agent should be gone — replaced by koda_docs skill.\nstderr: {clean}"
    );
}
