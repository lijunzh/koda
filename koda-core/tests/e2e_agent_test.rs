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
                "prompt": "review the auth module",
            }),
        ),
        MockResponse::Text("Sub-agent says: Echo: review the auth module".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // **#1163 (Lean A)**: every InvokeAgent dispatch returns a task_id
    // immediately. The sub-agent's actual output reaches the parent
    // via:
    //  (a) the per-task `ChildTaskUpdate` stream (`AgentStatus::Completed`
    //      carries `summary`, which IS the sub-agent's final output),
    //  (b) auto-drain on a future parent iteration, which injects the
    //      summary as a synthetic user message.
    // Pre-#1163 there was a third path — the sub-agent's output came
    // back in the `ToolCallResult.output` of the InvokeAgent call —
    // and the test asserted on that path. With the fg execution mode
    // deleted, `ToolCallResult.output` is now the spawn confirmation
    // string ("Background agent '...' started (agent:N)..."). We
    // pivot the assertion to path (a) since it's the most direct.
    assert!(
        events.iter().any(
            |e| matches!(e, EngineEvent::SubAgentStart { agent_name } if agent_name == "echo-agent")
        ),
        "expected SubAgentStart for echo-agent (now hoisted to dispatch \
         time, fires regardless of inline-vs-spawn path), got: {events:#?}"
    );

    let bg_events = env
        .collect_bg_events_after(events, Duration::from_secs(10))
        .await
        .expect("echo-agent never reached terminal state within 10s");
    runtime_env::remove("KODA_MOCK_RESPONSES");

    let completed_summary = bg_events.iter().find_map(|e| match e {
        EngineEvent::ChildTaskUpdate {
            status: koda_core::child_agent::AgentStatus::Completed { summary },
            ..
        } => Some(summary.clone()),
        _ => None,
    });
    assert!(
        completed_summary
            .as_deref()
            .is_some_and(|s| s.contains("Echo: review the auth module")),
        "echo-agent's Completed summary should carry its echoed output; \
         got summary = {completed_summary:?}, bg_events = {bg_events:#?}"
    );

    // The parent's second mock response ("Sub-agent says: ...") still
    // fires — the parent's loop pulls it on iter 2 once it has the
    // spawn-confirmation tool result. The DB-side check holds.
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
                "prompt": "do the thing",
            }),
        ),
        MockResponse::Text("parent done".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // **#1163 (Lean A)**: dispatch is now spawn-and-return, so
    // `run_inference` returns BEFORE the sub-agent has created its
    // DB session and finished writing assistant rows. Wait for
    // terminal status before reading DB state — otherwise the
    // `list_sessions` lookup races the sub-agent's session insert
    // and finds only the parent. (Pre-#1163 the foreground inline
    // path made this race-free "by accident" — dispatch blocked
    // until the sub-agent fully completed.)
    let _bg_events = env
        .collect_bg_events_after(events, Duration::from_secs(10))
        .await
        .expect("loop-test-agent never reached terminal state within 10s");
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
        // **#1163 (Lean A)**: post-PR every InvokeAgent dispatch is
        // spawn-and-return, so iter 1's `sub_agent_cache.put` lands
        // *inside* the spawned task asynchronously. Without a barrier
        // here, iter 2 (the would-be cache hit) races iter 1's
        // bg agent and almost always wins on Linux — result: cache
        // miss, second spawn, no `cache hit` Info event, test fails.
        //
        // Pre-#1325 Phase 5b this test used `WaitTask(["agent:1"])`
        // for the barrier. Phase 5b retired `WaitTask`; the equivalent
        // serialization now goes through `WaitForMail` — the mailbox
        // bridge from #1336 (`notify_parent_mailbox`) sends a
        // completion mail to the parent the moment the bg agent
        // exits, AFTER `cache.put` has run (same function body,
        // return Ok comes after the put). `WaitForMail` blocks the
        // current turn until that mail arrives, giving us the same
        // happens-before edge.
        MockResponse::tool_call("WaitForMail", serde_json::json!({})),
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
///
/// **#1163 (Lean A)**: every InvokeAgent dispatch is now a background
/// spawn that returns immediately with a task_id. The parent's
/// `run_inference` therefore returns BEFORE the sub-agent has called
/// its mock provider — reading `take_env_calls()` straight after would
/// race and observe an empty slice. We poll the bg-agent registry for
/// terminal status before reading the recorded calls so the assertion
/// site sees the full conversation regardless of scheduler timing.
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
    let events = env.run_inference(&provider).await;
    // Wait for the bg sub-agent to reach a terminal state — pre-#1163
    // the inline foreground path completed inside `run_inference`, so
    // `take_env_calls()` could be read immediately. Now we have to
    // synchronize with the spawned task ourselves.
    let _ = env
        .collect_bg_events_after(events, Duration::from_secs(10))
        .await;
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

    // **#1163 (Lean A) regression guard**: pre-#1163 the helper
    // returned before the sub-agent had called its provider, so this
    // assertion could pass vacuously on an empty `calls` vec (the
    // sentinel-absence check below is trivially true on no content).
    // The helper now waits for terminal bg status, so an empty
    // `calls` vec genuinely means the sub-agent never ran — fail loud.
    assert!(
        !calls.is_empty(),
        "sub-agent provider should have been called — the helper waits \
         for terminal bg status, so an empty vec means the spawn never \
         drove its provider (real failure, not a race)."
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
            serde_json::json!({"agent_name": "would-recurse", "prompt": "go", "background": false}),
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

    // **#1163 (Lean A)**: pre-#1163 the sub-agent ran inline on the
    // foreground path, so its `Info`, `ToolCallStart`, and final
    // `ToolCallResult` events all landed on the parent's sink and
    // could be inspected directly. Now the sub-agent runs inside a
    // `tokio::spawn`-ed `run_bg_agent`, with a `BufferingSink` wrapped
    // in a `ForwardingChildSink` (#1201 B). Live tool/info events
    // surface as `ChildAgentActivity` on the parent's sink (drained
    // via the registry's status-event queue), and the final output
    // surfaces as the `summary` on `AgentStatus::Completed`. We
    // collect both before asserting.
    let bg_events = env
        .collect_bg_events_after(events.clone(), Duration::from_secs(10))
        .await
        .expect("spinner never reached terminal state within 10s");
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // The visible UX signal of a triggered grace turn: an `Info` line
    // with the "reached max turns" message. Pre-#1163 it was a raw
    // `EngineEvent::Info`; now it's wrapped as `ChildAgentActivity
    // { kind: Info { message } }`. If neither fires, the grace-turn
    // code path was skipped — #1135 regression.
    let grace_event_seen = bg_events.iter().chain(events.iter()).any(|e| match e {
        EngineEvent::ChildAgentActivity {
            kind: koda_core::engine::event::ChildAgentActivityKind::Info { message },
            ..
        } => message.contains("reached max turns"),
        EngineEvent::Info { message } => message.contains("reached max turns"),
        _ => false,
    });
    assert!(
        grace_event_seen,
        "expected an Info line announcing the grace turn (raw or forwarded as \
         ChildAgentActivity); got bg_events: {bg_events:#?}"
    );

    // The sub-agent's final output (grace-turn text) used to flow
    // back via `ToolCallResult.output`; post-#1163 it's the `summary`
    // on `AgentStatus::Completed`. Asserting on the partial-findings
    // string locks in the expected happy path.
    let summary = bg_events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ChildTaskUpdate {
                status: koda_core::child_agent::AgentStatus::Completed { summary },
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .expect("spinner must reach Completed status with a summary");
    assert!(
        summary.contains("Partial findings"),
        "sub-agent should return the grace-turn text in summary; got: {summary}"
    );

    // Belt-and-suspenders: verify the loop did NOT issue a 4th
    // `Glob` tool-call. Pre-#1163 these were `ToolCallStart` events
    // with `is_sub_agent: true`; now they're forwarded as
    // `ChildAgentActivity { kind: ToolStart { tool_name: "Glob", .. } }`.
    // Counts >= 4 mean the grace turn's own tool call leaked through
    // to dispatch.
    let glob_call_count = bg_events
        .iter()
        .chain(events.iter())
        .filter(|e| {
            matches!(
                e,
                EngineEvent::ChildAgentActivity {
                    kind: koda_core::engine::event::ChildAgentActivityKind::ToolStart { tool_name, .. },
                    ..
                } if tool_name == "Glob"
            )
        })
        .count();
    assert_eq!(
        glob_call_count, 3,
        "expected exactly 3 Glob dispatches (iters 1-3); grace turn must not dispatch tools. \
         got {glob_call_count} ChildAgentActivity::ToolStart(Glob) events; bg_events: {bg_events:#?}"
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

    // **#1163 (Lean A)**: see sister test for the migration rationale.
    // The sub-agent's final marker now lives on `AgentStatus::Completed.summary`
    // and its `Glob` tool calls are forwarded as `ChildAgentActivity`.
    let bg_events = env
        .collect_bg_events_after(events.clone(), Duration::from_secs(10))
        .await
        .expect("defiant never reached terminal state within 10s");
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // Marker present in the bg agent's Completed summary.
    let summary = bg_events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ChildTaskUpdate {
                status: koda_core::child_agent::AgentStatus::Completed { summary },
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .expect("defiant must reach Completed status with a summary");
    assert!(
        summary.contains("[max_turns reached"),
        "expected marker for defiant grace turn in summary; got: {summary}"
    );

    // Only 2 Glob dispatches (iter 1-2). Iter 3 was the grace turn —
    // its tool call must be dropped, not dispatched. Counts via the
    // forwarded `ChildAgentActivity::ToolStart` (post-#1163) since
    // sub-agent tool events no longer surface as raw `ToolCallStart`
    // on the parent sink.
    let glob_call_count = bg_events
        .iter()
        .chain(events.iter())
        .filter(|e| {
            matches!(
                e,
                EngineEvent::ChildAgentActivity {
                    kind: koda_core::engine::event::ChildAgentActivityKind::ToolStart { tool_name, .. },
                    ..
                } if tool_name == "Glob"
            )
        })
        .count();
    assert_eq!(
        glob_call_count, 2,
        "expected exactly 2 Glob dispatches; the grace turn's tool call \
         must be dropped, not executed. got: {glob_call_count}; bg_events: {bg_events:#?}"
    );
}

// ── #1232 §3a: pre-flight context-budget check ──────────────────────────────

/// When the resolved sub-agent context (system + tools + prompt) exceeds the
/// model's `max_context_tokens`, dispatch must bail BEFORE any LLM call with
/// an actionable breakdown — not let the user see a raw upstream 400.
///
/// We trigger the gate by setting `max_context_tokens: 50` in the agent
/// JSON. The system prompt + 1 inherited tool + the user prompt easily
/// blow past 50 tokens, so the pre-flight tripwire fires on the first
/// iteration. (`max_context_tokens` is per-agent, not parent-inherited
/// — it comes from `ModelSettings::defaults_for(model)` unless the agent
/// JSON overrides it.)
///
/// Asserts:
///   * the sub-agent's `InvokeAgent` tool result mentions the over-budget
///     condition (the actionable error the model sees on its next turn);
///   * a `ChildAgentActivity` activity message tagged with the pre-flight
///     summary is emitted so the overlay (#1232 §1) shows the failure
///     reason instead of just a silent "agent finished" row;
///   * NO mock provider call was ever made — the gate fires before
///     `provider.chat(...)`. Pre-PR, the dispatch would have plowed ahead
///     and the user would have seen the upstream 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_preflight_bails_when_context_exceeds_window() {
    let _lock = ENV_MUTEX.lock().await;
    // Sub-agent's `max_context_tokens` does NOT inherit from the parent
    // (config.rs builds it from `ModelSettings::defaults_for(model)`).
    // Set the budget directly in the agent JSON so the gate has a knob
    // to trip on.
    let env = Env::new().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("over-budget-agent.json"),
        serde_json::json!({
            "name": "over-budget-agent",
            "system_prompt": "You are a moderately verbose agent. \
                              Take your time, think step by step, and \
                              explain your reasoning carefully before \
                              answering. This system prompt alone is \
                              already large enough to blow a tiny \
                              context window many times over.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0",
            // 50-token budget is small enough that the system prompt
            // alone busts it. Pre-flight tripwire fires first iteration.
            "max_context_tokens": 50
        })
        .to_string(),
    )
    .unwrap();

    // If the gate fails to fire, this MockProvider would be hit and the
    // test would still pass for the wrong reason. Set a sentinel response
    // that, if observed in the final transcript, proves the bypass.
    runtime_env::set(
        "KODA_MOCK_RESPONSES",
        r#"[{"text": "BYPASSED_PREFLIGHT_GATE"}]"#,
    );
    env.insert_user_message("delegate to the over-budget agent")
        .await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "over-budget-agent",
                "prompt": "do the thing",
            }),
        ),
        MockResponse::Text("done".into()),
    ]);
    let events = env.run_inference(&provider).await;

    // **#1163 (Lean A)**: pre-flight runs inside `run_bg_agent`'s
    // spawned task now (not on the parent's inline path), so the
    // `Info { "context pre-flight failed" }` event lands on the bg
    // agent's `BufferingSink` and is forwarded as `ChildAgentActivity
    // { kind: Info { message } }`. Final output (the actionable
    // error) lands on `AgentStatus::Errored.error` (preflight bails
    // with `Err`, not `Ok`). Wait for terminal status before asserting.
    let bg_events = env
        .collect_bg_events_after(events.clone(), Duration::from_secs(10))
        .await
        .expect("over-budget-agent never reached terminal state within 10s");
    runtime_env::remove("KODA_MOCK_RESPONSES");

    // 1. The pre-flight Info event must surface so the overlay shows
    //    the failure reason. Pin substrings the dispatcher emits.
    //    Accept either the raw form (legacy parent-sink path, kept as
    //    a defensive check) OR the forwarded ChildAgentActivity form.
    let preflight_signal = bg_events.iter().chain(events.iter()).any(|e| match e {
        EngineEvent::ChildAgentActivity {
            kind: koda_core::engine::event::ChildAgentActivityKind::Info { message },
            ..
        } => message.contains("context pre-flight failed"),
        EngineEvent::Info { message } => message.contains("context pre-flight failed"),
        _ => false,
    });
    assert!(
        preflight_signal,
        "expected pre-flight failed Info event (raw or forwarded as \
         ChildAgentActivity); got bg_events: {bg_events:#?}"
    );

    // 2. The sub-agent's actionable error must land on the bg agent's
    //    terminal status — either as `Errored.error` (the typical
    //    bail path) or as the `Completed.summary` (if the dispatcher
    //    chose to surface it as a graceful completion). Accept both.
    let terminal_text = bg_events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ChildTaskUpdate {
                status: koda_core::child_agent::AgentStatus::Errored { error },
                ..
            } => Some(error.clone()),
            EngineEvent::ChildTaskUpdate {
                status: koda_core::child_agent::AgentStatus::Completed { summary },
                ..
            } => Some(summary.clone()),
            _ => None,
        })
        .expect("over-budget-agent must reach a terminal status with surfaced text");
    assert!(
        terminal_text.contains("context exceeds model window")
            || terminal_text.contains("pre-flight"),
        "terminal text must surface the pre-flight bail; got: {terminal_text}"
    );

    // 3. The sub-agent's MockProvider must NOT have been hit. If our
    //    sentinel ever appears in the assistant transcript, the gate
    //    was bypassed.
    let last = env
        .db
        .last_assistant_message(&env.session_id)
        .await
        .unwrap_or_default();
    assert!(
        !last.contains("BYPASSED_PREFLIGHT_GATE"),
        "sub-agent's mock provider was reached \u{2014} pre-flight gate bypassed! transcript: {last}"
    );
}

/// Negative case: a generously-budgeted sub-agent must NOT be tripped by
/// the pre-flight gate. Catches a regression where the heuristic over-counts
/// and starts blocking legitimately-sized invocations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_agent_preflight_passes_under_normal_budget() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::builder().max_context_tokens(200_000).build().await;

    let agents_dir = env.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("normal-agent.json"),
        serde_json::json!({
            "name": "normal-agent",
            "system_prompt": "You are helpful.",
            "allowed_tools": [],
            "provider": "mock",
            "base_url": "http://localhost:0"
        })
        .to_string(),
    )
    .unwrap();

    runtime_env::set("KODA_MOCK_RESPONSES", r#"[{"text": "ok"}]"#);
    env.insert_user_message("delegate to normal-agent").await;

    let provider = MockProvider::new(vec![
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "agent_name": "normal-agent",
                "prompt": "hi",
                "background": false
            }),
        ),
        MockResponse::Text("done".into()),
    ]);
    let events = env.run_inference(&provider).await;
    runtime_env::remove("KODA_MOCK_RESPONSES");

    let preflight_failed = events.iter().any(|e| {
        matches!(
            e,
            EngineEvent::Info { message } if message.contains("context pre-flight failed")
        )
    });
    assert!(
        !preflight_failed,
        "200k budget must not trip the gate \u{2014} regression! got events: {events:?}"
    );
}

// ── #1232 §5: required `agent_name` ─────────────────────────────────────────

/// When the model emits `InvokeAgent` without `agent_name`, dispatch must
/// surface an actionable validation error (with the available-agent list)
/// instead of silently routing to `task` — the bug-review session showed
/// 10/10 calls hit the silent-default path, so every "Rust code architect"
/// / "security specialist" prompt was actually answered by the generic
/// worker.
///
/// Asserts:
///   * the InvokeAgent tool result is an error string mentioning
///     `'agent_name' is required` (the runtime backstop bailed);
///   * the error string lists at least one built-in agent name
///     (so the model can self-correct on the next turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invoke_agent_without_agent_name_is_rejected_with_actionable_error() {
    let _lock = ENV_MUTEX.lock().await;
    let env = Env::new().await;

    env.insert_user_message("call without agent_name").await;

    let provider = MockProvider::new(vec![
        // No `agent_name` field — the silently-defaulted path pre-#1232 §5.
        MockResponse::tool_call(
            "InvokeAgent",
            serde_json::json!({
                "prompt": "do something",
                "background": false,
            }),
        ),
        MockResponse::Text("acknowledged".into()),
    ]);
    let events = env.run_inference(&provider).await;

    let invoke_result = events
        .iter()
        .find_map(|e| match e {
            EngineEvent::ToolCallResult { name, output, .. } if name == "InvokeAgent" => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("InvokeAgent tool result must be present");

    assert!(
        invoke_result.contains("'agent_name' is required"),
        "result must surface the required-field error; got: {invoke_result}"
    );
    // The hint must list available agents so the model can self-correct.
    // Don't pin a specific name beyond `task` (always present as a built-in)
    // — discovery output is environment-sensitive.
    assert!(
        invoke_result.contains("task"),
        "error must list `task` in the available agents hint; got: {invoke_result}"
    );
}
