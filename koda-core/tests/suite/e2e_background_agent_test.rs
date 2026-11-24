//! E2E tests: background sub-agents, as in Claude Code
//! (`InvokeAgent { run_in_background: true }`, `SendMessage`, `TaskStop`).

use std::time::Duration;

use koda_core::engine::EngineEvent;
use koda_test_utils::network::FakeLlmServer;
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};

fn write_agent_config(env: &Env, server: &FakeLlmServer, name: &str) {
    let agents_dir = env.root.join(".koda/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join(format!("{name}.json")),
        serde_json::json!({
            "name": name,
            "system_prompt": "You are a lean test agent.",
            "skip_memory": true,
            "allowed_tools": [],
            "provider": "lmstudio",
            "model": "stub",
            "base_url": server.url()
        })
        .to_string(),
    )
    .unwrap();
}

fn result_of(events: &[EngineEvent], tool: &str) -> String {
    events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ToolCallResult { name, output, .. } if name == tool => {
                Some(output.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {tool} result in {events:#?}"))
}

/// The agent id in a start or continue message.
fn agent_id(output: &str) -> String {
    output
        .split_whitespace()
        .find(|w| koda_core::tools::bg_agents::BgAgents::is_agent_id(w))
        .unwrap_or_else(|| panic!("no agent id in {output:?}"))
        .to_string()
}

/// Wait until no background agent runs. Its notice may already be in the
/// conversation: a fast agent finishes before the parent's next step, which
/// then takes it.
async fn wait_idle(env: &Env) {
    let agents = env.tools.bg_registry.agents();
    for _ in 0..400 {
        if agents.running() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the background agent never finished: {:?}",
        agents.snapshot()
    );
}

/// One more step, which delivers a notice still pending.
async fn next_step(env: &Env) {
    let provider = MockProvider::new(vec![MockResponse::Text("noted".into())]);
    env.run_inference(&provider).await;
}

/// The agent notices in the conversation.
async fn notices_in(env: &Env) -> Vec<String> {
    messages(env)
        .await
        .into_iter()
        .filter(|m| m.contains("<agent_notification"))
        .collect()
}

async fn messages(env: &Env) -> Vec<String> {
    use koda_state::session_store::SessionStore;
    env.db
        .load_context(&env.session_id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| m.content)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_agent_returns_at_once_and_reports_by_notice() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;
    let server = FakeLlmServer::spawn().await;
    write_agent_config(&env, &server, "echo-agent");
    server
        .mount_script_json(r#"[{"text": "Echo: the auth module is fine"}]"#)
        .await;

    env.insert_user_message("check auth in the background")
        .await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "echo-agent",
                "prompt": "review the auth module",
                "run_in_background": true,
            }),
        ),
        MockResponse::Text("Started; I'll report when it's done.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    let started = result_of(&events, "InvokeAgent");
    assert!(started.contains("in the background"), "{started}");
    let id = agent_id(&started);

    wait_idle(&env).await;
    assert!(!env.tools.bg_registry.any_working());

    // A step tells the model, once.
    next_step(&env).await;
    next_step(&env).await;
    let all = notices_in(&env).await;
    assert_eq!(all.len(), 1, "told once: {all:#?}");
    let notice = &all[0];
    assert!(notice.contains(&format!("id=\"{id}\"")), "{notice}");
    assert!(notice.contains("status=\"completed\""), "{notice}");
    assert!(notice.contains("Echo: the auth module is fine"), "{notice}");
    assert!(!env.tools.bg_registry.has_notices());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_message_continues_a_finished_agent_with_its_conversation() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;
    let server = FakeLlmServer::spawn().await;
    write_agent_config(&env, &server, "echo-agent");
    server
        .mount_script_json(r#"[{"text": "first answer"}, {"text": "second answer"}]"#)
        .await;

    env.insert_user_message("go").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "echo-agent",
                "prompt": "look at A",
                "run_in_background": true,
            }),
        ),
        MockResponse::Text("waiting".into()),
    ]);
    let id = agent_id(&result_of(
        &env.run_inference(&provider).await,
        "InvokeAgent",
    ));
    wait_idle(&env).await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "SendMessage",
            serde_json::json!({"to": id, "message": "now look at B"}),
        ),
        MockResponse::Text("asked it".into()),
    ]);
    let events = env.run_inference(&provider).await;
    assert!(
        result_of(&events, "SendMessage").contains("continues in the background"),
        "{events:#?}"
    );
    wait_idle(&env).await;
    next_step(&env).await;
    let all = notices_in(&env).await;
    // One notice per run. (The test's small window may compact the
    // conversation, so they aren't checked in order.)
    assert_eq!(all.len(), 2, "one notice per run: {all:#?}");
    assert!(
        all.iter().all(|n| n.contains(&format!("id=\"{id}\""))),
        "{all:#?}"
    );
    for answer in ["first answer", "second answer"] {
        assert_eq!(
            all.iter().filter(|n| n.contains(answer)).count(),
            1,
            "{answer}: {all:#?}"
        );
    }

    // The second run saw the first: its request carries both prompts and
    // the first answer.
    let calls = server.chat_messages().await;
    assert_eq!(calls.len(), 2, "{calls:#?}");
    let second = serde_json::to_string(&calls[1]).unwrap();
    for part in ["look at A", "first answer", "now look at B"] {
        assert!(second.contains(part), "{part} missing from {second}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_stop_stops_a_background_agent_without_a_notice() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;
    let server = FakeLlmServer::spawn().await;
    write_agent_config(&env, &server, "slow-agent");
    // Its model takes far longer than the test.
    server
        .mount_chat_delayed(
            Duration::from_secs(30),
            serde_json::json!({"choices": [{"message": {"role": "assistant", "content": "late"}}]}),
        )
        .await;

    env.insert_user_message("go").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "slow-agent",
                "prompt": "take your time",
                "run_in_background": true,
            }),
        ),
        MockResponse::Text("started".into()),
    ]);
    let id = agent_id(&result_of(
        &env.run_inference(&provider).await,
        "InvokeAgent",
    ));
    assert!(env.tools.bg_registry.any_working());

    let provider = MockProvider::new(vec![
        MockResponse::tool_call("TaskStop", serde_json::json!({"task_id": id})),
        MockResponse::Text("stopped it".into()),
    ]);
    let events = env.run_inference(&provider).await;
    assert!(
        result_of(&events, "TaskStop").contains("Stopped background agent"),
        "{events:#?}"
    );
    assert!(!env.tools.bg_registry.any_working());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !env.tools.bg_registry.has_notices(),
        "a stopped agent isn't reported"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_message_to_an_unknown_agent_says_so() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;
    env.insert_user_message("go").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "SendMessage",
            serde_json::json!({"to": "a0123456789ab", "message": "hello"}),
        ),
        MockResponse::Text("ok".into()),
    ]);
    let events = env.run_inference(&provider).await;
    let out = result_of(&events, "SendMessage");
    assert!(out.contains("No background agent a0123456789ab"), "{out}");
}
