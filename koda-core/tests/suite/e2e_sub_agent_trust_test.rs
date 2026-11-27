//! A sub-agent declared with `trust: "safe"` can write, and can't run a
//! destructive command nobody approved.
//!
//! A sub-agent's calls are judged by the session's mode and rules, as Claude
//! Code's sub-agents inherit the session's mode. What would ask the main agent
//! asks in the main session (`approval_flow::Approver`); with nobody to ask, as
//! in these tests, it is refused. (#1250 had sub-agents auto-approve edits and
//! block destructive commands instead, since they couldn't ask.)
//!
//! Run with:
//!   cargo test -p koda-core \
//!              --test e2e_sub_agent_trust_test

use koda_core::{engine::EngineEvent, trust::TrustMode};
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};

/// Helper: write a sub-agent JSON declaring `trust: "safe"`,
/// matching the post-#1250 built-in `task` agent shape.
fn write_safe_trust_sub_agent(env: &Env, server: &FakeLlmServer, name: &str) {
    let agents_dir = env.root.join(".koda/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{name}.json")),
        serde_json::json!({
            "name": name,
            "system_prompt": "You are a write-capable sub-agent for testing.",
            "allowed_tools": [],
            "trust": "safe",
            "provider": "lmstudio",
            "model": "stub",
            "base_url": server.url()
        })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_trust_sub_agent_can_write_files() {
    // The session is in Auto, so the sub-agent's Write runs without asking,
    // whatever the agent declares (Claude Code applies an agent's own mode
    // only when the session is in its default mode).
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    write_safe_trust_sub_agent(&env, &server, "writer");

    // Sub-agent script: emit a Write tool call, then a final text reply.
    // The sub-agent's requests go to `server`, which plays this script.
    let target_path = env.root.join("sub_agent_wrote_this.txt");
    let target_str = target_path.to_string_lossy().to_string();
    let sub_responses = serde_json::json!([
        {
            "tool": "Write",
            "args": { "file_path": target_str, "content": "hello from sub-agent" }
        },
        { "text": "Done writing." }
    ]);
    server.mount_script_json(&sub_responses.to_string()).await;

    env.insert_user_message("delegate a write to the writer agent")
        .await;

    // Parent: invoke the sub-agent, then reply with a final text turn.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "writer",
                "prompt": "create the test file",
                "background": false,
            }),
        ),
        MockResponse::Text("Sub-agent reported: Done writing.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    // **#1366 phase 1 (sync sub-agent dispatch)**: dispatch is
    // synchronous — by the time `run_inference` returns the sub-
    // agent has already played its script, dispatched
    // its Write tool call, and persisted its DB rows. The deflake
    // wait from #1321/#1323/#1327 is no longer needed because the
    // race window it guarded against (parent-loop termination vs
    // bg-agent completion) does not exist on the sync path.

    // Assertion 1: the file actually exists on disk. This is the
    // strongest possible proof that the Write was approved and
    // executed end-to-end through the sub-agent dispatch loop.
    assert!(
        target_path.exists(),
        "sub-agent's Write must produce the file at {target_str:?} \
         in an Auto session"
    );
    let written = std::fs::read_to_string(&target_path).unwrap();
    assert_eq!(written, "hello from sub-agent");

    // Assertion 2: no rejection event in the stream. If the matrix
    // had returned NeedsConfirmation and dispatch had auto-rejected,
    // there'd be a tool-call result containing the no-channel error.
    let invoke_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { name, output, .. } = e
            && name == "InvokeAgent"
        {
            return Some(output.clone());
        }
        None
    });
    let invoke_output = invoke_result.expect("InvokeAgent must produce a result");
    assert!(
        !invoke_output.contains("no channel to the user"),
        "sub-agent must NOT see the no-channel rejection (pre-#1250 \
         regression). Got: {invoke_output}"
    );
    assert!(
        !invoke_output.contains("requires user confirmation"),
        "sub-agent must NOT see a confirmation-required error \
         (Auto runs it without asking). Got: {invoke_output}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_trust_sub_agent_destructive_bash_is_still_blocked() {
    // A destructive command asks even in Safe; with nobody to ask it is
    // refused, and the file survives. Filesystem state is the proof: the
    // sub-agent's last reply says nothing about each call.
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Auto).build().await;
    let server = FakeLlmServer::spawn().await;
    write_safe_trust_sub_agent(&env, &server, "destroyer");

    // Pre-create the file the sub-agent will try to delete.
    let target = env.root.join("do_not_delete.txt");
    std::fs::write(&target, "survive me").unwrap();
    assert!(
        target.exists(),
        "setup: target file must exist before sub-agent runs"
    );

    let target_str = target.to_string_lossy().to_string();
    let sub_responses = serde_json::json!([
        {
            "tool": "Bash",
            "args": { "command": format!("rm -rf {target_str}") }
        },
        { "text": "Tried to delete." }
    ]);
    server.mount_script_json(&sub_responses.to_string()).await;

    env.insert_user_message("delegate a destructive op to the destroyer agent")
        .await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "destroyer",
                "prompt": "delete things",
                "background": false,
            }),
        ),
        MockResponse::Text("Done.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    let _ = events;
    // **#1366 phase 1 (sync sub-agent dispatch)**: dispatch is
    // synchronous — by the time `run_inference` returns the sub-
    // agent has already finished its loop, so any destructive op
    // (or its block) is already on disk. The pre-#1366 deflake
    // wait is no longer needed.

    // The destructive op MUST have been blocked. Filesystem state
    // is the unforgeable proof: had it run,
    // `rm -rf` would have removed the file.
    assert!(
        target.exists(),
        "an unapproved destructive Bash from a sub-agent must not run; \
         the target file was deleted"
    );
    let surviving = std::fs::read_to_string(&target).unwrap();
    assert_eq!(surviving, "survive me", "file content must be intact");
}
