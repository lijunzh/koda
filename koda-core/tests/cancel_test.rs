//! Regression test: Ctrl+C must interrupt inference even before the first token.
//!
//! Issue: when a slow model (e.g., local LM Studio) takes seconds to return
//! the HTTP response headers, `chat_stream().await` blocks and ignores the
//! cancellation token. The fix wraps that await in `tokio::select!` against
//! `cancel.cancelled()`.

use anyhow::Result;
use async_trait::async_trait;
use koda_core::persistence::Persistence;
use koda_core::{
    config::{KodaConfig, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent},
    inference::{self, InferenceContext},
    providers::{LlmResponse, ModelInfo},
    tools::ToolRegistry,
};
use koda_test_utils::{ChatMessage, LlmProvider, TestSink, ToolDefinition};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A mock provider that signals when `chat_stream` is entered, then
/// sleeps forever — simulating a model that hangs on the initial
/// HTTP request. The signal lets tests cancel deterministically as
/// soon as inference reaches the provider, rather than guessing a
/// wall-clock delay (#1109 F3).
struct SlowProvider {
    /// Fired exactly once when `chat_stream` is first invoked.
    entered: Mutex<Option<oneshot::Sender<()>>>,
}

impl SlowProvider {
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                entered: Mutex::new(Some(tx)),
            },
            rx,
        )
    }
}

#[async_trait]
impl LlmProvider for SlowProvider {
    async fn chat(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &koda_core::config::ModelSettings,
    ) -> Result<LlmResponse> {
        unreachable!("should not be called in streaming mode")
    }

    async fn chat_stream(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        _settings: &koda_core::config::ModelSettings,
    ) -> Result<koda_core::providers::stream_collector::SseCollector> {
        // Notify the test that we've reached the provider — the earliest
        // meaningful cancellation point. Tests subscribe to this signal
        // instead of guessing a wall-clock delay.
        if let Some(tx) = self.entered.lock().unwrap().take() {
            let _ = tx.send(());
        }
        // Simulate a model that hangs on the initial HTTP request.
        tokio::time::sleep(Duration::from_secs(60)).await;
        unreachable!("should be cancelled before this returns")
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![])
    }

    fn provider_name(&self) -> &str {
        "slow-test"
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cancel_during_chat_stream_returns_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::init(tmp.path()).await.unwrap();
    let session_id = db.create_session("test-agent", tmp.path()).await.unwrap();

    // Insert a user message so inference has something to send
    db.insert_message(&session_id, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    let (provider, entered_rx) = SlowProvider::new();
    let tools = ToolRegistry::new(PathBuf::from("."), 100_000);
    let tool_defs: Vec<ToolDefinition> = vec![];
    let sink = TestSink::new();
    let cancel = CancellationToken::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let mut file_tracker = koda_core::file_tracker::FileTracker::new(&session_id, db.clone()).await;

    // **#1109 F3**: was `sleep(100ms).await` to give inference time to
    // start. Replaced with a oneshot signal so cancel fires the moment
    // inference reaches the provider — deterministic, immune to slow CI.
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let _ = entered_rx.await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();

    let result = inference::inference_loop(InferenceContext {
        project_root: &PathBuf::from("."),
        config: &config,
        db: &db,
        session_id: &session_id,
        system_prompt: "You are a test assistant.",
        provider: &provider,
        tools: &tools,
        tool_defs: &tool_defs,
        pending_images: None,
        mode: koda_core::trust::TrustMode::Auto,
        sink: &sink,
        cancel,
        cmd_rx: &mut cmd_rx,
        file_tracker: &mut file_tracker,
        bg_agents: &koda_core::child_agent::new_shared(),
        sub_agent_cache: &koda_core::sub_agent_cache::SubAgentCache::new(),
        agent_path: &koda_core::agent::AgentPath::root(),
    })
    .await;

    let elapsed = start.elapsed();

    // Must return Ok (graceful cancellation, not an error)
    assert!(result.is_ok(), "inference_loop should return Ok on cancel");

    // Must complete quickly — not wait for the 60s sleep
    assert!(
        elapsed < Duration::from_secs(2),
        "should cancel in <2s, took {elapsed:?}"
    );

    // Should have emitted Warn("Interrupted")
    let events = sink.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::Warn { message } if message == "Interrupted")),
        "should emit Interrupted warning, got: {events:?}"
    );
}
