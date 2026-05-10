//! E2E pin for the bug fix in #1250: a sub-agent declared with
//! `trust: "safe"` (the new task-style declaration that replaces
//! `write_access: true`) can actually write files.
//!
//! Pre-#1250 this scenario silently failed: the sub-agent dispatch
//! loop creates a dead approval channel by design (no human watches
//! it), so any tool that returned `NeedsConfirmation` from the trust
//! matrix auto-rejected with *"requires user confirmation but this
//! sub-agent has no channel to the user."*  At Safe trust that
//! included Write/Edit/Delete — so a `task` agent invoked at Safe
//! could not write at all, even though its purpose is to write.
//!
//! Post-#1250: `check_tool_for_sub_agent` resolves `NeedsConfirmation`
//! via the safe-side rule (mutating → AutoApprove, destructive →
//! Blocked), so a Safe-trust sub-agent's Write succeeds without
//! confirmation while `rm -rf` is still rejected.
//!
//! Run with:
//!   cargo test -p koda-core --features test-support \
//!              --test e2e_sub_agent_trust_test

use koda_core::{engine::EngineEvent, runtime_env, trust::TrustMode};
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};

/// Helper: write a sub-agent JSON declaring `trust: "safe"`,
/// matching the post-#1250 built-in `task` agent shape.
fn write_safe_trust_sub_agent(env: &Env, name: &str) {
    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{name}.json")),
        serde_json::json!({
            "name": name,
            "system_prompt": "You are a write-capable sub-agent for testing.",
            "allowed_tools": [],
            "trust": "safe",
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_trust_sub_agent_can_write_files() {
    // **Bug fix pin for #1250 / #1249**: pre-fix this would auto-reject
    // the Write tool with "requires user confirmation but this sub-agent
    // has no channel to the user." Post-fix the safe-side rule auto-
    // approves the Write because mutating ops at Safe in sub-agent
    // context default to AutoApprove.
    let _lock = ENV_MUTEX.lock().await;
    // Parent at Safe trust (the production scenario where the bug
    // surfaced — running `koda --mode safe` and delegating to `task`).
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    write_safe_trust_sub_agent(&env, "writer");

    // Sub-agent script: emit a Write tool call, then a final text reply.
    // The sub-agent's MockProvider reads from KODA_MOCK_RESPONSES.
    let target_path = env.root.join("sub_agent_wrote_this.txt");
    let target_str = target_path.to_string_lossy().to_string();
    let sub_responses = serde_json::json!([
        {
            "tool": "Write",
            "args": { "file_path": target_str, "content": "hello from sub-agent" }
        },
        { "text": "Done writing." }
    ]);
    runtime_env::set("KODA_MOCK_RESPONSES", sub_responses.to_string());

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
    // agent has already consumed `KODA_MOCK_RESPONSES`, dispatched
    // its Write tool call, and persisted its DB rows. The deflake
    // wait from #1321/#1323/#1327 is no longer needed because the
    // race window it guarded against (parent-loop termination vs
    // bg-agent completion) does not exist on the sync path.
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // Assertion 1: the file actually exists on disk. This is the
    // strongest possible proof that the Write was approved and
    // executed end-to-end through the sub-agent dispatch loop.
    assert!(
        target_path.exists(),
        "sub-agent's Write must produce the file at {target_str:?} \
         (pre-#1250 this would have auto-rejected with no-channel error)"
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
         (the safe-side rule should auto-approve). Got: {invoke_output}"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safe_trust_sub_agent_destructive_bash_is_still_blocked() {
    // Companion to the bug-fix test above: even though Safe-trust
    // sub-agents can now write, **destructive** operations remain
    // blocked at all trust levels in the sub-agent context. This
    // pins the asymmetry that's the whole point of the safe-side
    // rule: "default to safe" means yes for mutating, no for
    // destructive.
    //
    // Detection strategy: have the sub-agent try to `rm -rf` a file
    // we just created. If the trust matrix is doing its job, the
    // file survives — regardless of what the sub-agent's final
    // narrative says (the InvokeAgent result is the sub-agent's last
    // text reply, not a per-tool-error log, so we can't sniff for
    // refusal strings). Filesystem state is the unforgeable proof.
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().trust(TrustMode::Safe).build().await;
    write_safe_trust_sub_agent(&env, "destroyer");

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
    runtime_env::set("KODA_MOCK_RESPONSES", sub_responses.to_string());

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
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // The destructive op MUST have been blocked. Filesystem state
    // is the unforgeable proof: if the matrix had auto-approved,
    // `rm -rf` would have removed the file.
    assert!(
        target.exists(),
        "destructive Bash from a Safe-trust sub-agent MUST be blocked. \
         The target file was deleted, which means the safe-side rule \
         failed to refuse a destructive op. This is the #1250 invariant."
    );
    let surviving = std::fs::read_to_string(&target).unwrap();
    assert_eq!(surviving, "survive me", "file content must be intact");
}
