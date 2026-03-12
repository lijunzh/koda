//! Session lifecycle tests: TurnStart/TurnEnd events, cancellation, and DB persistence.
//!
//! Uses MockProvider + TestSink (requires the `test-support` feature).
//! Run with: `cargo test -p koda-core --features koda-core/test-support`

use anyhow::Result;
use async_trait::async_trait;
use koda_core::{
    approval::ApprovalMode,
    config::{KodaConfig, ProviderType},
    db::{Database, Persistence, Role},
    engine::{EngineCommand, EngineEvent, event::TurnEndReason, sink::TestSink},
    providers::{
        ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, ToolDefinition,
        mock::{MockProvider, MockResponse},
    },
    session::KodaSession,
    settings::Settings,
    tools::ToolRegistry,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── Shared test harness ──────────────────────────────────────────────────────

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    db: Database,
    session_id: String,
    config: KodaConfig,
}

impl Env {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = Database::init(&root).await.unwrap();
        let session_id = db.create_session("test-agent", &root).await.unwrap();
        let config = KodaConfig::default_for_testing(ProviderType::Mock);
        Self {
            _tmp: tmp,
            root,
            db,
            session_id,
            config,
        }
    }

    async fn insert_user_message(&self, text: &str) {
        self.db
            .insert_message(&self.session_id, &Role::User, Some(text), None, None, None)
            .await
            .unwrap();
    }

    /// Build a KodaSession, injecting an arbitrary provider.
    ///
    /// Bypasses `KodaSession::new()` (which calls `create_provider` internally)
    /// so tests can supply a pre-configured MockProvider.  A fresh ToolRegistry
    /// is created each call because ToolRegistry does not implement Clone.
    fn make_session(&self, provider: Box<dyn LlmProvider>) -> (KodaSession, CancellationToken) {
        let cancel = CancellationToken::new();
        let tools = ToolRegistry::new(self.root.clone(), self.config.max_context_tokens);

        let agent = Arc::new(koda_core::agent::KodaAgent {
            project_root: self.root.clone(),
            tools,
            tool_defs: ToolRegistry::new(self.root.clone(), self.config.max_context_tokens)
                .get_definitions(&[]),
            system_prompt: "You are a test assistant.".to_string(),
            mcp_registry: Arc::new(tokio::sync::RwLock::new(koda_core::mcp::McpRegistry::new())),
            mcp_statuses: vec![],
        });

        // Wire the DB+session into the ToolRegistry so RecallContext works.
        agent
            .tools
            .set_session(Arc::new(self.db.clone()), self.session_id.clone());

        let session = KodaSession {
            id: self.session_id.clone(),
            agent,
            db: self.db.clone(),
            provider,
            mode: ApprovalMode::Auto,
            settings: Settings::load(),
            cancel: cancel.clone(),
        };
        (session, cancel)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// `run_turn()` must emit `TurnStart` as the first event and
/// `TurnEnd { reason: Complete }` as the last event after a successful turn.
#[tokio::test]
async fn session_run_turn_emits_turn_start_and_end() {
    let env = Env::new().await;
    env.insert_user_message("say hello").await;

    let provider = Box::new(MockProvider::new(vec![MockResponse::Text(
        "Hello!".to_string(),
    )]));
    let (mut session, _cancel) = env.make_session(provider);

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    let result = session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await;
    assert!(
        result.is_ok(),
        "run_turn should succeed: {:?}",
        result.err()
    );

    let events = sink.events();

    // TurnStart must be the first event emitted.
    let first = events.first().expect("expected at least one event");
    assert!(
        matches!(first, EngineEvent::TurnStart { .. }),
        "first event must be TurnStart, got: {first:?}"
    );

    // TurnEnd must be the last event emitted.
    let last = events.last().expect("expected at least one event");
    assert!(
        matches!(last, EngineEvent::TurnEnd { .. }),
        "last event must be TurnEnd, got: {last:?}"
    );

    // Reason must be Complete on a successful turn.
    if let EngineEvent::TurnEnd { reason, .. } = last {
        assert_eq!(
            *reason,
            TurnEndReason::Complete,
            "TurnEnd reason should be Complete after successful turn"
        );
    }

    // The turn_id in TurnStart and TurnEnd must match.
    let start_id = if let EngineEvent::TurnStart { turn_id } = first {
        turn_id.clone()
    } else {
        unreachable!()
    };
    let end_id = if let EngineEvent::TurnEnd { turn_id, .. } = last {
        turn_id.clone()
    } else {
        unreachable!()
    };
    assert_eq!(
        start_id, end_id,
        "TurnStart and TurnEnd must share the same turn_id"
    );
}

/// Cancelling the token during inference causes `TurnEnd` to carry reason `Cancelled`.
#[tokio::test]
async fn session_cancellation_produces_turn_end_cancelled() {
    let env = Env::new().await;
    env.insert_user_message("hello").await;

    // A provider that hangs forever so that cancellation can be observed.
    struct HangingProvider;

    #[async_trait]
    impl LlmProvider for HangingProvider {
        async fn chat(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<LlmResponse> {
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _: &[ChatMessage],
            _: &[ToolDefinition],
            _: &koda_core::config::ModelSettings,
        ) -> Result<mpsc::Receiver<StreamChunk>> {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            unreachable!()
        }
        async fn list_models(&self) -> Result<Vec<ModelInfo>> {
            Ok(vec![])
        }
        fn provider_name(&self) -> &str {
            "hanging"
        }
    }

    let (mut session, cancel) = env.make_session(Box::new(HangingProvider));

    // Cancel after 100 ms so the test completes quickly.
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let sink = TestSink::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);

    let start = std::time::Instant::now();
    let result = session
        .run_turn(&env.config, None, &sink, &mut cmd_rx)
        .await;

    let elapsed = start.elapsed();
    assert!(
        result.is_ok(),
        "cancellation should be graceful, not an error"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "cancellation should unblock quickly, took {elapsed:?}"
    );

    let events = sink.events();

    // A TurnEnd event must be present with reason Cancelled.
    let turn_end_reason = events.iter().find_map(|e| {
        if let EngineEvent::TurnEnd { reason, .. } = e {
            Some(reason.clone())
        } else {
            None
        }
    });
    assert!(
        turn_end_reason.is_some(),
        "expected a TurnEnd event after cancellation, got: {events:?}"
    );
    assert_eq!(
        turn_end_reason.unwrap(),
        TurnEndReason::Cancelled,
        "TurnEnd reason must be Cancelled when the token is cancelled"
    );
}

/// Messages from two separate `run_turn()` calls on the same session must
/// both appear in the DB afterward.
#[tokio::test]
async fn session_persists_messages_across_two_turns() {
    let env = Env::new().await;

    // --- Turn 1 ---
    env.insert_user_message("first question").await;

    let provider1 = Box::new(MockProvider::new(vec![MockResponse::Text(
        "first answer".to_string(),
    )]));
    let (mut session1, _cancel1) = env.make_session(provider1);
    let sink1 = TestSink::new();
    let (_, mut cmd_rx1) = mpsc::channel::<EngineCommand>(1);
    session1
        .run_turn(&env.config, None, &sink1, &mut cmd_rx1)
        .await
        .expect("turn 1 should succeed");

    // Verify turn 1 ended with Complete.
    assert!(
        sink1.events().iter().any(|e| matches!(
            e,
            EngineEvent::TurnEnd {
                reason: TurnEndReason::Complete,
                ..
            }
        )),
        "turn 1 should end with Complete"
    );

    // --- Turn 2 ---
    env.insert_user_message("second question").await;

    let provider2 = Box::new(MockProvider::new(vec![MockResponse::Text(
        "second answer".to_string(),
    )]));
    // A new KodaSession sharing the same DB and session_id represents the
    // continuation of the conversation after, e.g., a model swap.
    let (mut session2, _cancel2) = env.make_session(provider2);
    let sink2 = TestSink::new();
    let (_, mut cmd_rx2) = mpsc::channel::<EngineCommand>(1);
    session2
        .run_turn(&env.config, None, &sink2, &mut cmd_rx2)
        .await
        .expect("turn 2 should succeed");

    // Verify both turns' messages are in the DB.
    let messages: Vec<koda_core::persistence::Message> =
        env.db.load_context(&env.session_id, 100_000).await.unwrap();
    let contents: Vec<String> = messages
        .iter()
        .filter_map(|m: &koda_core::persistence::Message| m.content.clone())
        .collect();

    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("first question")),
        "DB should contain first user message"
    );
    assert!(
        contents.iter().any(|c: &String| c.contains("first answer")),
        "DB should contain first assistant response"
    );
    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("second question")),
        "DB should contain second user message"
    );
    assert!(
        contents
            .iter()
            .any(|c: &String| c.contains("second answer")),
        "DB should contain second assistant response"
    );
}
