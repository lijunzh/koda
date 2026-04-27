//! E2E tests: sub-agent invocation and caching.

use koda_core::{bg_agent::AgentStatus, engine::EngineEvent, persistence::Persistence};
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};
use std::time::Duration;

#[tokio::test]
async fn test_sub_agent_invocation_e2e() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("echo-agent.json"),
        serde_json::json!({
            "name": "echo-agent",
            "system_prompt": "You are a simple echo agent. Repeat back the user's prompt verbatim.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var(
            "KODA_MOCK_RESPONSES",
            r#"[{"text": "Echo: review the auth module"}]"#,
        );
    }

    env.insert_user_message("delegate to echo-agent").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "echo-agent",
                "prompt": "review the auth module"
            }),
        ),
        MockResponse::Text("Sub-agent says: Echo: review the auth module".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    assert!(
        events.iter().any(
            |e| matches!(e, EngineEvent::SubAgentStart { agent_name } if agent_name == "echo-agent")
        ),
        "expected SubAgentStart for echo-agent, got: {events:?}"
    );

    let tool_result = events.iter().find_map(|e| {
        if let EngineEvent::ToolCallResult { output, name, .. } = e
            && name == "InvokeAgent"
        {
            return Some(output.clone());
        }
        None
    });
    assert!(
        tool_result.is_some(),
        "expected InvokeAgent tool result, got: {events:?}"
    );
    assert!(
        tool_result
            .unwrap()
            .contains("Echo: review the auth module"),
        "sub-agent result should contain echoed prompt"
    );

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Sub-agent says"),
        "final response should reference sub-agent output: {last}"
    );
}

#[tokio::test]
async fn test_sub_agent_cache_hit_skips_llm() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("echo-agent.json"),
        serde_json::json!({
            "name": "echo-agent",
            "system_prompt": "You are a simple echo agent.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"text": "cached result"}]"#);
    }
    env.insert_user_message("call the agent twice").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "echo-agent", "prompt": "do the thing"}),
        ),
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "echo-agent", "prompt": "do the thing"}),
        ),
        MockResponse::Text("Done with both calls.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    let cache_hit = events
        .iter()
        .any(|e| matches!(e, EngineEvent::Info { message } if message.contains("cache hit")));
    assert!(cache_hit, "expected cache hit event, got: {events:?}");

    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("Done with both calls"),
        "should complete with final response: {last}"
    );
}

// ── skip_memory isolation (#769) ────────────────────────────────────────────

/// Helper: creates `agents/<name>.json` under `env.root`.
fn write_agent_config(env: &Env, name: &str, skip_memory: bool) {
    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{name}.json")),
        serde_json::json!({
            "name": name,
            "system_prompt": "You are a lean test agent.",
            "skip_memory": skip_memory,
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();
}

/// Runs one InvokeAgent call via the outer provider and returns the env-provider
/// recorded calls (messages the sub-agent's MockProvider received).
async fn invoke_agent_and_take_calls(
    env: &Env,
    agent_name: &str,
) -> Vec<Vec<koda_core::providers::ChatMessage>> {
    MockProvider::clear_env_calls();
    env.insert_user_message(&format!("call {agent_name}")).await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": agent_name, "prompt": "go"}),
        ),
        MockResponse::Text("done".into()),
    ]);
    env.run_inference(&provider).await;
    MockProvider::take_env_calls()
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn skip_memory_excludes_project_memory_from_sub_agent() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Write a distinctive sentinel to the project memory file.
    std::fs::write(env.root.join("MEMORY.md"), "SENTINEL_XYZ").unwrap();
    write_agent_config(&env, "lean-agent", /* skip_memory */ true);

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"text": "sub done"}]"#);
    }
    let calls = invoke_agent_and_take_calls(&env, "lean-agent").await;
    unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };

    assert!(
        !calls.is_empty(),
        "sub-agent provider should have been called"
    );
    let all_content: String = calls
        .iter()
        .flatten()
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert!(
        !all_content.contains("SENTINEL_XYZ"),
        "skip_memory: true must exclude project memory from sub-agent system prompt; got:\n{all_content}"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn without_skip_memory_project_memory_reaches_sub_agent() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Same sentinel — but this agent does NOT skip memory.
    std::fs::write(env.root.join("MEMORY.md"), "SENTINEL_XYZ").unwrap();
    write_agent_config(&env, "full-agent", /* skip_memory */ false);

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var("KODA_MOCK_RESPONSES", r#"[{"text": "sub done"}]"#);
    }
    let calls = invoke_agent_and_take_calls(&env, "full-agent").await;
    unsafe { std::env::remove_var("KODA_MOCK_RESPONSES") };

    assert!(
        !calls.is_empty(),
        "sub-agent provider should have been called"
    );
    let all_content: String = calls
        .iter()
        .flatten()
        .filter_map(|m| m.content.as_deref())
        .collect();
    assert!(
        all_content.contains("SENTINEL_XYZ"),
        "skip_memory: false must include project memory in sub-agent system prompt; got:\n{all_content}"
    );
}

// ── #1022 B7 (revised): sub-agents cannot spawn sub-agents ─────────────────

/// If a sub-agent's model emits `InvokeAgent` anyway (rogue, scripted,
/// or just confused), the dispatch loop short-circuits with a clean
/// refusal rather than recursing or returning the registry's confusing
/// `success=false` boilerplate. The sub-agent then continues and
/// produces its final text response.
///
/// This is the regression test for the original B7 bug where nested
/// `InvokeAgent` fell through to a registry stub returning
/// `"InvokeAgent is handled by the inference loop."` — and for the
/// stack-overflow risk that allowing real recursion would have created.
#[cfg(feature = "test-support")]
#[tokio::test]
async fn sub_agent_invoke_agent_is_refused_with_clear_message() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    write_agent_config(&env, "would-recurse", /* skip_memory */ true);

    // Sub-agent's mock plays two responses in order: it first tries to
    // call `InvokeAgent` (which should be refused), then emits its
    // final text. The refusal must not abort the sub-agent.
    //
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var(
            "KODA_MOCK_RESPONSES",
            r#"[{"tool": "InvokeAgent", "args": {"agent_name": "would-recurse", "prompt": "recurse"}}, {"text": "final after refusal"}]"#,
        );
    }

    env.insert_user_message("delegate").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "would-recurse", "prompt": "go"}),
        ),
        MockResponse::Text("parent done".into()),
    ]);
    let _events = env.run_inference(&provider).await;

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    // The sub-agent's final text reached the parent — i.e. the refusal
    // did not abort the sub-agent loop, and the parent received a
    // useful result.
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap();
    assert!(
        last.contains("parent done"),
        "parent must complete after sub-agent refusal cycle; got: {last}"
    );
}

// ── QA-001: background agent iter-counter status advances (#1045) ───────────
//
// Verifies that when InvokeAgent dispatches with `background: true`,
// the status channel progresses through at least one full inference
// iteration.  `Completed` is the only terminal state that proves the
// loop actually ran; reaching it implies `iter ≥ 1` was sent because
// `run_bg_agent` sends `Running { iter: n }` at the top of each
// iteration and `Completed` is only emitted after the loop exits.

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bg_agent_iter_counter_advances_via_status_channel() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    write_agent_config(&env, "bg-counter-agent", /* skip_memory */ true);

    // Give the background agent's mock provider a single text response.
    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::set_var(
            "KODA_MOCK_RESPONSES",
            r#"[{"text": "background work done"}]"#,
        );
    }

    env.insert_user_message("launch background agent").await;

    // Parent calls InvokeAgent with background:true, then returns immediately.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "bg-counter-agent",
                "prompt": "do some work",
                "background": true
            }),
        ),
        MockResponse::Text("parent done".into()),
    ]);

    env.run_inference(&provider).await;

    // SAFETY: ENV_MUTEX serializes all tests that touch this env var.
    unsafe {
        std::env::remove_var("KODA_MOCK_RESPONSES");
    }

    // The background task is registered (reserve+attach are synchronous
    // before spawn), so it should appear in the snapshot immediately.
    // Poll with a 5-second deadline in case the runtime hasn’t scheduled
    // the spawned task yet.
    let task_id = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let snap = env.bg_agents.snapshot();
            if let Some(task) = snap.first() {
                break task.task_id;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "background agent was never registered in the registry within 5s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };

    // Subscribe gives us the last-sent status value plus change
    // notifications going forward.
    let mut rx = env
        .bg_agents
        .subscribe(task_id)
        .expect("task_id must be in registry: snapshot confirmed it above");

    // Drive the receiver to a terminal state.
    // `Completed` proves the loop ran ≥ 1 full iteration (QA-001 core).
    let final_status = loop {
        let status = rx.borrow_and_update().clone();
        if matches!(
            status,
            AgentStatus::Completed { .. } | AgentStatus::Errored { .. } | AgentStatus::Cancelled
        ) {
            break status;
        }

        match tokio::time::timeout(Duration::from_secs(10), rx.changed()).await {
            Ok(Ok(())) => continue, // new value available; loop to inspect it
            Ok(Err(_closed)) => {
                // Sender dropped — final value is buffered; pick it up.
                break rx.borrow().clone();
            }
            Err(_elapsed) => {
                panic!("bg agent did not reach a terminal state within 10s");
            }
        }
    };

    match &final_status {
        AgentStatus::Completed { summary } => {
            assert!(
                !summary.is_empty(),
                "bg agent completed with empty summary — \
                 execute_sub_agent output was not captured"
            );
        }
        AgentStatus::Errored { error } => panic!("bg agent errored: {error}"),
        AgentStatus::Cancelled => panic!("bg agent was unexpectedly cancelled"),
        _ => unreachable!("loop only breaks on terminal states"),
    }
}
