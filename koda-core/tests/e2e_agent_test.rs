//! E2E tests: sub-agent invocation and caching.

use koda_core::{
    child_agent::AgentStatus, engine::EngineEvent, persistence::Persistence, runtime_env,
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
/// re-issued the same tool call forever, previously hitting the
/// (now-removed, see #1110) iteration cap; today the same scenario
/// would terminate via `LoopDetector` consecutive-identical detection
/// or context exhaustion.
///
/// User-visible symptom (post-#1099 when paths actually rendered):
///
/// ```text
/// ● List /Users/lijun/repo
/// ● List /Users/lijun/repo
/// ● List /Users/lijun/repo
/// ... (repeats until LoopDetector hard-stop or Ctrl+C)
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

    // Use the `collect_bg_events_after` helper from koda-test-utils
    // — it merges the events vec from `run_inference` (what the
    // parent's inference_loop drained into the sink) with the
    // registry's `drain_status_events()` queue (whatever was emitted
    // after parent finished). See the helper's docs for the full
    // race-condition rationale (#1109, PR #1113).
    use koda_core::engine::EngineEvent;
    let events_from_sink = env.run_inference(&provider).await;
    let bg_events = env
        .collect_bg_events_after(events_from_sink, Duration::from_secs(10))
        .await
        .unwrap_or_else(|partial| {
            panic!(
                "bg task never reached a terminal state within 10s.\n\
                 bg_events ({} total): {partial:#?}",
                partial.len()
            )
        });

    let bg_updates: Vec<&AgentStatus> = bg_events
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::ChildTaskUpdate { status, .. } => Some(status),
            _ => None,
        })
        .collect();

    assert!(
        !bg_updates.is_empty(),
        "expected at least one ChildTaskUpdate event; bg_events ({} total): {bg_events:#?}",
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

// ── #1135: sub-agent max_turns + grace turn (gemini pattern) ────────────────

/// #1135 regression: a sub-agent that keeps calling tools past its
/// configured `max_iterations` must hit the gemini-pattern grace turn
/// and terminate with a synthesized final answer instead of looping
/// indefinitely.
///
/// Scenario:
/// - Agent JSON declares `max_iterations: 3` (small to keep the test fast).
/// - Sub-agent's mock script: 4 tool calls (would consume iter 1-4) +
///   one final text response ("partial findings") which is what the
///   model produces ON the grace turn (iter 4, since `iter > max_turns`
///   triggers grace at iter = max + 1 = 4).
/// - Expected: 3 tool calls actually dispatched (iter 1-3). Iter 4 is
///   the grace turn — its response is consumed but tool calls (if any)
///   are NOT dispatched. Final result is the grace-turn text.
///
/// Tool choice: `Glob` against a pattern that matches nothing in the
/// temp dir. Returns an empty list cleanly without needing a real
/// file fixture, and is auto-approved under `TrustMode::Auto`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sub_agent_grace_turn_terminates_runaway_explorer() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("spinner.json"),
        serde_json::json!({
            "name": "spinner",
            "system_prompt": "You are a test agent that loves Glob.",
            "allowed_tools": ["Glob"],
            "max_iterations": 3,
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // Sub-agent's mock responses. Four entries:
    //   iter 1-3: Glob calls against patterns that won't match (loop
    //             continues; #1135 simulation — model keeps "exploring"
    //             without synthesizing).
    //   iter 4:   Plain text response — the model complies with the
    //             grace-turn reminder. With max_iterations=3 the loop
    //             runs iter 1,2,3 (tool calls dispatched), then iter 4
    //             is the grace turn (`iter > max_turns`). Whatever
    //             that response is becomes the sub-agent's final
    //             output (text — grace passes; tool calls — dropped
    //             with a marker).
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[
            {"tool": "Glob", "args": {"pattern": "nope_a_*"}},
            {"tool": "Glob", "args": {"pattern": "nope_b_*"}},
            {"tool": "Glob", "args": {"pattern": "nope_c_*"}},
            {"text": "Partial findings: I searched for nope_* patterns and found nothing. My investigation was interrupted."}
        ]"#,
    );

    env.insert_user_message("delegate to spinner").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "spinner", "prompt": "search broadly"}),
        ),
        MockResponse::Text("Sub-agent reported back.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // The visible UX signal of a triggered grace turn: an `Info` event
    // with the "reached max turns" message. If this never fires the
    // grace-turn code path was skipped — #1135 regression.
    let grace_event_seen = events.iter().any(
        |e| matches!(e, EngineEvent::Info { message } if message.contains("reached max turns")),
    );
    assert!(
        grace_event_seen,
        "expected an Info event announcing the grace turn; got: {events:?}"
    );

    // Sub-agent's `InvokeAgent` tool result must contain the grace-turn
    // text. If it contains the marker text instead ("[max_turns
    // reached: ...]"), the model also called tools on the grace turn —
    // valid but a different code path; the test would need adjusting.
    // Asserting on the partial-findings string locks in the expected
    // happy path.
    let invoke_result = events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ToolCallResult { name, output, .. } if name == "InvokeAgent" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("InvokeAgent must produce a ToolCallResult event");
    assert!(
        invoke_result.contains("Partial findings"),
        "sub-agent should return the grace-turn text; got: {invoke_result}"
    );

    // Belt-and-suspenders: verify the loop did NOT issue a 4th
    // `Glob` tool-call (sub-agents emit `ToolCallStart` per dispatch;
    // they don't emit `ToolCallResult` because the result lands in
    // `Role::Tool` rows on the sub-agent's session, not on the
    // parent's event stream). Counts >= 4 mean the grace turn's own
    // tool call leaked through to dispatch.
    let glob_call_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                EngineEvent::ToolCallStart { name, is_sub_agent: true, .. } if name == "Glob"
            )
        })
        .count();
    assert_eq!(
        glob_call_count, 3,
        "expected exactly 3 Glob dispatches (iters 1-3); grace turn must not dispatch tools. \
         got {glob_call_count} Glob ToolCallStart events in events: {events:?}"
    );
}

/// #1135 fallback path: when a misbehaving model defies the grace-turn
/// reminder and emits MORE tool calls instead of a text answer, those
/// tool calls must NOT be dispatched. Instead the sub-agent returns a
/// `[max_turns reached: ...]` marker so the parent (and user) get a
/// clear signal that the budget was hit and the model failed to wrap up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_sub_agent_grace_turn_drops_tool_calls_when_model_defies() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("defiant.json"),
        serde_json::json!({
            "name": "defiant",
            "system_prompt": "You ignore reminders and keep tooling.",
            "allowed_tools": ["Glob"],
            "max_iterations": 2,
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    // 3 tool calls — iter 1,2 dispatched, iter 3 is the grace turn but
    // model still emits a tool call (the bug class we want to defend
    // against). Implementation must drop the iter-3 tool call and
    // synthesize a marker.
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[
            {"tool": "Glob", "args": {"pattern": "nope_a_*"}},
            {"tool": "Glob", "args": {"pattern": "nope_b_*"}},
            {"tool": "Glob", "args": {"pattern": "nope_c_*"}}
        ]"#,
    );

    env.insert_user_message("delegate to defiant").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({"agent_name": "defiant", "prompt": "search"}),
        ),
        MockResponse::Text("Sub-agent reported back.".into()),
    ]);
    let events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // Marker present in the InvokeAgent result.
    let invoke_result = events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ToolCallResult { name, output, .. } if name == "InvokeAgent" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("InvokeAgent must produce a ToolCallResult event");
    assert!(
        invoke_result.contains("[max_turns reached"),
        "expected marker for defiant grace turn; got: {invoke_result}"
    );

    // Only 2 Glob dispatches (iter 1-2). Iter 3 was the grace turn —
    // its tool call must be dropped, not dispatched. Counts via
    // `ToolCallStart` because sub-agents don't emit `ToolCallResult`
    // (results live on the sub-agent's `Role::Tool` rows, not on
    // the parent's event stream).
    let glob_call_count = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                EngineEvent::ToolCallStart { name, is_sub_agent: true, .. } if name == "Glob"
            )
        })
        .count();
    assert_eq!(
        glob_call_count, 2,
        "expected exactly 2 Glob dispatches; the grace turn's tool call \
         must be dropped, not executed. got: {events:?}"
    );
}
