//! E2E tests: sub-agent invocation and caching.

mod e2e_harness;

use e2e_harness::{ENV_MUTEX, Env};
use koda_core::{
    engine::EngineEvent,
    persistence::Persistence,
    providers::mock::{MockProvider, MockResponse},
};

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
