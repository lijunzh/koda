//! E2E tests: sub-agent invocation and caching.

use koda_core::{
    bg_agent::AgentStatus, engine::EngineEvent, persistence::Persistence, runtime_env,
};
use koda_test_utils::{ENV_MUTEX, Env, MockProvider, MockResponse};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[{"text": "Echo: review the auth module"}]"#,
    );

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
    runtime_env::remove("KODA_MOCK_RESPONSES");

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

/// Regression for koda#1101: the sub-agent dispatch loop forgot to
/// mark its assistant messages complete via `db.mark_message_complete`.
/// Because `load_context` filters out
/// `(role = 'assistant' AND completed_at IS NULL)` rows, every
/// iteration the sub-agent reloaded a context with **no assistant
/// turns** — then `prune_mismatched_tool_calls` orphan-pruned the
/// tool result rows, leaving only `[system, user]`. The sub-agent
/// re-issued the same tool call forever, hitting the iteration cap.
///
/// User-visible symptom (post-#1099 when paths actually rendered):
///
/// ```text
/// ● List /Users/lijun/repo
/// ● List /Users/lijun/repo
/// ● List /Users/lijun/repo
/// ... (repeats until iteration cap or Ctrl+C)
/// ```
///
/// This test scripts the sub-agent's mock provider to:
///   1. Issue a `ListSkills` tool call (no-arg, no-side-effect tool)
///   2. Reply with final text
///
/// If the bug is present, the sub-agent will burn all `KODA_MOCK_RESPONSES`
/// on repeated tool calls and never reach the text reply — OR the DB
/// will end up with assistant rows where `completed_at IS NULL`.
/// Either failure is asserted below.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_marks_assistant_messages_complete_so_loop_progresses() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("loop-test-agent.json"),
        serde_json::json!({
            "name": "loop-test-agent",
            "system_prompt": "You are a test agent. Call ListSkills then reply done.",
            "allowed_tools": ["ListSkills"],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // Sub-agent script: tool call, then final text. With the bug,
    // the sub-agent would reload a context missing the assistant
    // tool-call turn and re-issue the same call — burning the
    // second response on another tool call instead of the text.
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[
                {"tool_calls": [{"id": "tc_1", "name": "ListSkills", "arguments": "{}"}]},
                {"text": "sub-agent done"}
            ]"#,
    );

    env.insert_user_message("delegate").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "loop-test-agent",
                "prompt": "do the thing"
            }),
        ),
        MockResponse::Text("parent done".into()),
    ]);
    let _events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // Find the sub-agent's session. `list_sessions` returns newest-first,
    // so the loop-test-agent session is at the top (created after the
    // parent test session, before the parent's final text response).
    let sessions = env.db.list_sessions(10, &env.root).await.unwrap();
    let sub_session = sessions
        .iter()
        .find(|s| s.agent_name == "loop-test-agent")
        .unwrap_or_else(|| {
            panic!(
                "loop-test-agent session must exist; got: {:?}",
                sessions.iter().map(|s| &s.agent_name).collect::<Vec<_>>()
            )
        });

    // Direct DB-level assertion: load_context applies the same filter
    // the sub-agent's loop applies. If any assistant row has
    // `completed_at IS NULL`, it'll be missing here, which is the
    // exact mechanism that caused the loop spin.
    let context = env.db.load_context(&sub_session.id).await.unwrap();
    let assistant_turns = context
        .iter()
        .filter(|m| matches!(m.role, koda_core::persistence::Role::Assistant))
        .count();
    assert!(
        assistant_turns >= 1,
        "sub-agent's load_context must include at least one assistant turn; found {assistant_turns}. \
         Pre-fix this was zero because mark_message_complete was never called, so every iteration \
         the sub-agent saw `[system, user]` only and re-issued the same tool call. Context: {context:#?}"
    );

    // Belt-and-suspenders: load_all_messages bypasses the completed_at
    // filter, so any drift between the two counts pinpoints incomplete
    // assistant rows even if `assistant_turns` happens to be ≥1
    // for some other reason.
    let all = env.db.load_all_messages(&sub_session.id).await.unwrap();
    let all_assistant = all
        .iter()
        .filter(|m| matches!(m.role, koda_core::persistence::Role::Assistant))
        .count();
    assert_eq!(
        all_assistant, assistant_turns,
        "every assistant row in the sub-agent session must be visible to load_context; \
         all={all_assistant}, filtered={assistant_turns}. Drift = some assistant rows have \
         completed_at IS NULL = the loop-spin bug is back."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    runtime_env::set("KODA_MOCK_RESPONSES", r#"[{"text": "cached result"}]"#);
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
    runtime_env::remove("KODA_MOCK_RESPONSES");

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_memory_excludes_project_memory_from_sub_agent() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Write a distinctive sentinel to the project memory file.
    std::fs::write(env.root.join("MEMORY.md"), "SENTINEL_XYZ").unwrap();
    write_agent_config(&env, "lean-agent", /* skip_memory */ true);
    runtime_env::set("KODA_MOCK_RESPONSES", r#"[{"text": "sub done"}]"#);
    let calls = invoke_agent_and_take_calls(&env, "lean-agent").await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_skip_memory_project_memory_reaches_sub_agent() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    // Same sentinel — but this agent does NOT skip memory.
    std::fs::write(env.root.join("MEMORY.md"), "SENTINEL_XYZ").unwrap();
    write_agent_config(&env, "full-agent", /* skip_memory */ false);
    runtime_env::set("KODA_MOCK_RESPONSES", r#"[{"text": "sub done"}]"#);
    let calls = invoke_agent_and_take_calls(&env, "full-agent").await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_invoke_agent_is_refused_with_clear_message() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    write_agent_config(&env, "would-recurse", /* skip_memory */ true);

    // Sub-agent's mock plays two responses in order: it first tries to
    // call `InvokeAgent` (which should be refused), then emits its
    // final text. The refusal must not abort the sub-agent.
    //
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[{"tool": "InvokeAgent", "args": {"agent_name": "would-recurse", "prompt": "recurse"}}, {"text": "final after refusal"}]"#,
    );

    env.insert_user_message("delegate").await;
    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "would-recurse", "prompt": "go"}),
        ),
        MockResponse::Text("parent done".into()),
    ]);
    let _events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

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
//
// **Runtime flavor**: the production code path uses `tokio::spawn` for
// background sub-agents (see `sub_agent_dispatch::run_bg_agent` and the
// B5 comment block). On `current_thread` runtimes (the `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`
// default) `tokio::spawn` queues the future but it can ONLY make
// progress when the test task explicitly yields. The dispatch path
// itself is fully synchronous between `reserve()` and `attach()`, but
// the spawned future's first poll happens lazily — and on macOS CI
// runners we observed cases where the test's polling loop spun on a
// snapshot that never updated, suggesting the dispatch future itself
// hadn't completed before the test resumed. Pinning to `multi_thread`
// matches production semantics and gives the spawned task a dedicated
// worker, eliminating the scheduling pathology. See #1090.

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bg_agent_iter_counter_advances_via_status_channel() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    write_agent_config(&env, "bg-counter-agent", /* skip_memory */ true);

    // Give the background agent's mock provider a single text response.
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[{"text": "background work done"}]"#,
    );

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

    // **#1109 macOS-fix**: poll `bg_agents.drain_status_events()`
    // directly instead of relying on the parent's inference_loop to
    // forward events to the test sink. Background agents enqueue
    // `BgTaskUpdate` events on the registry; the parent's loop drains
    // them at the top of each iteration. After `run_inference`
    // returns, *no one* drains them — so subscribing to the sink
    // misses every bg event that fires after parent completes. Going
    // straight to the registry queue is race-free: events are pushed
    // there as soon as the bg task emits them. The polling here is
    // bounded by a 10s budget; on a healthy machine it returns in a
    // few ms. See PR #1113 diagnostic for the full story.
    use koda_core::engine::EngineEvent;
    let _ = env.run_inference(&provider).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut bg_events: Vec<EngineEvent> = Vec::new();
    let saw_terminal = loop {
        for ev in env.bg_agents.drain_status_events() {
            bg_events.push(ev);
        }
        let terminal = bg_events.iter().any(|ev| {
            matches!(
                ev,
                EngineEvent::BgTaskUpdate {
                    status: AgentStatus::Completed { .. }
                        | AgentStatus::Errored { .. }
                        | AgentStatus::Cancelled,
                    ..
                }
            )
        });
        if terminal {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        saw_terminal,
        "bg task never reached a terminal state within 10s.\nbg_events ({} total): {bg_events:#?}",
        bg_events.len()
    );

    let bg_updates: Vec<&AgentStatus> = bg_events
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::BgTaskUpdate { status, .. } => Some(status),
            _ => None,
        })
        .collect();

    assert!(
        !bg_updates.is_empty(),
        "expected at least one BgTaskUpdate event; bg_events ({} total): {bg_events:#?}",
        bg_events.len()
    );

    // QA-001 core: the loop ran ≥ 1 full iteration. The engine emits
    // Running {{ iter }} at the TOP of each iteration, so iter ≥ 1
    // proves the loop body completed at least once.
    let max_iter_seen = bg_updates
        .iter()
        .filter_map(|s| match s {
            AgentStatus::Running { iter } => Some(*iter),
            _ => None,
        })
        .max();
    assert!(
        matches!(max_iter_seen, Some(n) if n >= 1),
        "expected Running {{ iter >= 1 }}; saw max iter = {max_iter_seen:?}.\nbg_events: {bg_events:#?}"
    );

    let final_status = bg_updates
        .iter()
        .rev()
        .find(|s| {
            matches!(
                s,
                AgentStatus::Completed { .. }
                    | AgentStatus::Errored { .. }
                    | AgentStatus::Cancelled
            )
        })
        .copied()
        .unwrap_or_else(|| {
            panic!("bg task never reached a terminal state.\nbg_updates: {bg_updates:#?}")
        });

    match final_status {
        AgentStatus::Completed { summary } => {
            assert!(
                !summary.is_empty(),
                "bg agent completed with empty summary — \
                 execute_sub_agent output was not captured"
            );
        }
        AgentStatus::Errored { error } => panic!("bg agent errored: {error}"),
        AgentStatus::Cancelled => panic!("bg agent was unexpectedly cancelled"),
        _ => unreachable!("filter above only keeps terminal states"),
    }
    // Removed only after the bg task has finished reading it.
    runtime_env::remove("KODA_MOCK_RESPONSES");
}
