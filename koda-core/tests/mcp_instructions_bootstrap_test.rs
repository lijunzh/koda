//! Bootstrap-level integration tests for #922: MCP server `instructions`
//! must reach the LLM in the actual production code path.
//!
//! These tests guard against the regression that #927 originally shipped:
//! the static `agent.system_prompt` was built BEFORE the `McpManager`
//! attached, so the per-server `instructions` block was always empty in
//! production despite passing unit tests.
//!
//! The fix (#929) composes the MCP block per-turn inside
//! `KodaSession::run_turn`, so the assertion this file makes is the
//! *exact* behavior users depend on: the system prompt sent to the
//! provider — not just what `build_system_prompt` returns in isolation —
//! must contain the per-server guidance.
//!
//! Run with: `cargo test -p koda-core --features test-support --test mcp_instructions_bootstrap_test`

use koda_core::{
    engine::EngineCommand,
    mcp::{
        McpManager,
        client::{McpClient, McpClientStatus},
        config::{McpServerConfig, McpTransport},
    },
    session::KodaSession,
    tools::ToolRegistry,
    trust::TrustMode,
};
use koda_test_utils::{ChatMessage, Env, MockProvider, MockResponse, TestSink};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

/// Stub config for tests — transport details don't matter because we use
/// `set_status_for_test` / `set_instructions_for_test` to bypass the real
/// connection lifecycle.
fn dummy_config() -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Stdio {
            command: "false".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
        },
        startup_timeout_sec: 1,
        tool_timeout_sec: 1,
        enabled_tools: None,
        disabled_tools: None,
    }
}

/// Build a `KodaSession` with the given pre-populated MCP manager attached
/// and return a handle to the provider's recorded calls so the test can
/// inspect what was sent to the model after the turn runs.
async fn make_session_with_mcp(
    env: &Env,
    responses: Vec<MockResponse>,
    mcp_manager: McpManager,
) -> (
    KodaSession,
    CancellationToken,
    Arc<Mutex<Vec<Vec<ChatMessage>>>>,
) {
    let cancel = CancellationToken::new();
    let tools = ToolRegistry::new(env.root.clone(), env.config.max_context_tokens);

    // Attach the MCP manager BEFORE wrapping in Arc — this matches the
    // production flow inside KodaSession::new() where set_mcp_manager()
    // runs before the agent is shared.
    tools.set_mcp_manager(Arc::new(RwLock::new(mcp_manager)));

    let agent = Arc::new(koda_core::agent::KodaAgent {
        project_root: env.root.clone(),
        tools,
        tool_defs: ToolRegistry::new(env.root.clone(), env.config.max_context_tokens)
            .get_definitions(&[], &[]),
        // Important: this is the STATIC prompt — it must NOT contain the
        // MCP block. The fix in #929 composes the MCP section per-turn in
        // `run_turn`, so this test verifies that composition actually
        // happens via the assertion on `recorded_calls()` below.
        system_prompt: "You are a test assistant.".to_string(),
    });

    agent
        .tools
        .set_session(Arc::new(env.db.clone()), env.session_id.clone());

    let file_tracker =
        koda_core::file_tracker::FileTracker::new(&env.session_id, env.db.clone()).await;

    let provider = MockProvider::new(responses);
    let recorded = provider.recorded_calls_handle();

    let session = KodaSession {
        id: env.session_id.clone(),
        agent,
        db: env.db.clone(),
        provider: Box::new(provider),
        mode: TrustMode::Auto,
        cancel: cancel.clone(),
        file_tracker,
        title_set: false,
        proxy: None,
        socks5_proxy: None,
        bg_agents: koda_core::bg_agent::new_shared(),
        sub_agent_cache: koda_core::sub_agent_cache::SubAgentCache::new(),
    };
    (session, cancel, recorded)
}

/// Extract the system prompt content from a recorded call (`messages[0]` is
/// conventionally the system role; we find it explicitly to be robust).
fn system_prompt_in(call: &[ChatMessage]) -> String {
    let system = call
        .iter()
        .find(|m| m.role == "system")
        .expect("expected a system-role message in the recorded call");
    system.content.clone().unwrap_or_default()
}

// ── The actual regression tests ─────────────────────────────────────────────

/// THE bootstrap test: a connected MCP server with non-empty `instructions`
/// must surface in the system prompt sent to the provider.
///
/// Before #929 this would have failed because `agent.system_prompt` was
/// built once with `&[]` and never refreshed.
#[tokio::test]
async fn mcp_instructions_reach_provider_in_live_turn() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // Build an MCP manager with one connected server that has instructions.
    let mut manager = McpManager::new();
    let mut client = McpClient::new("playwright".into(), dummy_config());
    client.set_status_for_test(McpClientStatus::Connected);
    client.set_instructions_for_test(Some(
        "Prefer locator-based queries over CSS selectors.".to_string(),
    ));
    manager.insert_client_for_test(client);

    let (mut session, _cancel, recorded) =
        make_session_with_mcp(&env, vec![MockResponse::Text("ok".into())], manager).await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await
        .expect("turn should succeed");

    let calls = recorded.lock().unwrap().clone();
    assert!(!calls.is_empty(), "provider received no calls");
    let prompt = system_prompt_in(&calls[0]);

    assert!(
        prompt.contains("# MCP Server Instructions"),
        "system prompt sent to provider must contain MCP header.\n\
         Got prompt:\n{prompt}"
    );
    assert!(
        prompt.contains("Prefer locator-based queries"),
        "system prompt must contain the server's actual instructions text"
    );
    assert!(
        prompt.contains("---[start of server instructions from playwright]---"),
        "must include provenance framing (security: prevents server impersonating koda mandates)"
    );
    assert!(
        prompt.contains("---[end of server instructions from playwright]---"),
        "must include provenance closing marker"
    );
}

/// A session with no MCP manager attached must not include any MCP block
/// in the system prompt — zero token cost for non-MCP users.
#[tokio::test]
async fn no_mcp_manager_means_no_mcp_block_in_prompt() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // Empty manager (no clients, but still attached) — equivalent to "no
    // MCP servers configured".
    let manager = McpManager::new();

    let (mut session, _cancel, recorded) =
        make_session_with_mcp(&env, vec![MockResponse::Text("ok".into())], manager).await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await
        .expect("turn should succeed");

    let calls = recorded.lock().unwrap().clone();
    let prompt = system_prompt_in(&calls[0]);
    assert!(
        !prompt.contains("# MCP Server Instructions"),
        "no connected MCP servers → no MCP block in prompt"
    );
}

/// A connected server that returned no `instructions` (None or empty) must
/// not produce an empty `## <server>` block — the renderer filters at the
/// client level via `.filter(|s| !s.trim().is_empty())`.
#[tokio::test]
async fn connected_server_without_instructions_is_omitted() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    let mut manager = McpManager::new();
    let mut client = McpClient::new("silent".into(), dummy_config());
    client.set_status_for_test(McpClientStatus::Connected);
    // Explicitly None — server returned no `instructions` field.
    client.set_instructions_for_test(None);
    manager.insert_client_for_test(client);

    let (mut session, _cancel, recorded) =
        make_session_with_mcp(&env, vec![MockResponse::Text("ok".into())], manager).await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await
        .expect("turn should succeed");

    let calls = recorded.lock().unwrap().clone();
    let prompt = system_prompt_in(&calls[0]);
    assert!(
        !prompt.contains("# MCP Server Instructions"),
        "no server provided instructions → no MCP block"
    );
    assert!(
        !prompt.contains("from silent"),
        "silent server must not produce a framed block"
    );
}

/// Hot-reload regression: a server that connects AFTER the agent's static
/// prompt is built must still surface in the next turn's prompt. This is
/// the original #922 bug: previously `rebuild_system_prompt` had to be
/// called manually and wasn't, so late-connecting servers were invisible.
#[tokio::test]
async fn server_connected_after_agent_built_still_appears_in_next_turn() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // Start with an empty manager — simulates "no MCP at agent build time".
    let manager = McpManager::new();
    let (mut session, _cancel, recorded) = make_session_with_mcp(
        &env,
        vec![
            MockResponse::Text("first".into()),
            MockResponse::Text("second".into()),
        ],
        manager,
    )
    .await;

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    // Turn 1: no MCP, no block.
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await
        .expect("turn 1 should succeed");
    {
        let calls = recorded.lock().unwrap().clone();
        let prompt = system_prompt_in(&calls[0]);
        assert!(!prompt.contains("# MCP Server Instructions"));
    }

    // Now simulate `/mcp add` connecting a new server mid-session by
    // mutating the manager that `agent.tools` already holds via Arc.
    {
        let mgr_arc = session.agent.tools.mcp_manager().expect("manager attached");
        let mut guard = mgr_arc.write().await;
        let mut client = McpClient::new("late".into(), dummy_config());
        client.set_status_for_test(McpClientStatus::Connected);
        client.set_instructions_for_test(Some("Late-connecting guidance.".to_string()));
        guard.insert_client_for_test(client);
    }

    // Queue a follow-up user message and run turn 2.
    env.insert_user_message("anything new?").await;
    session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await
        .expect("turn 2 should succeed");

    // Turn 2 (the most recent recorded call) MUST contain the late server.
    let calls = recorded.lock().unwrap().clone();
    let last_call = calls.last().expect("expected at least one call");
    let prompt = system_prompt_in(last_call);
    assert!(
        prompt.contains("# MCP Server Instructions"),
        "after hot-reload, MCP block must appear in the next turn"
    );
    assert!(
        prompt.contains("Late-connecting guidance."),
        "late-connecting server's instructions must surface"
    );
}
