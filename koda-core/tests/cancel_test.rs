//! Regression test: Ctrl+C must interrupt inference even before the first token.
//!
//! Issue: when a slow model (e.g., local LM Studio) takes seconds to return
//! the HTTP response headers, `chat_stream().await` blocks and ignores the
//! cancellation token. The fix wraps that await in `tokio::select!` against
//! `cancel.cancelled()`.

use anyhow::Result;
use async_trait::async_trait;
use koda_core::{
    config::{KodaConfig, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent, sink::TestSink},
    inference,
    providers::{ChatMessage, LlmProvider, LlmResponse, ModelInfo, StreamChunk, ToolDefinition},
    tools::ToolRegistry,
};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A mock provider that sleeps forever in `chat_stream`, simulating
/// a model that never returns (or takes very long to start streaming).
struct SlowProvider;

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
    ) -> Result<mpsc::Receiver<StreamChunk>> {
        // Simulate a model that hangs on the initial HTTP request
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

#[tokio::test]
async fn test_cancel_during_chat_stream_returns_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Database::init(tmp.path(), tmp.path()).await.unwrap();
    let session_id = db.create_session("test-agent", tmp.path()).await.unwrap();

    // Insert a user message so inference has something to send
    db.insert_message(&session_id, &Role::User, Some("hello"), None, None, None)
        .await
        .unwrap();

    let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    let provider = SlowProvider;
    let tools = ToolRegistry::new(PathBuf::from("."), 100_000);
    let tool_defs: Vec<ToolDefinition> = vec![];
    let sink = TestSink::new();
    let cancel = CancellationToken::new();
    let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
    let mut settings = koda_core::approval::Settings::load();

    // Cancel after 100ms — well before SlowProvider's 60s sleep
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = std::time::Instant::now();

    let result = inference::inference_loop(
        &PathBuf::from("."),
        &config,
        &db,
        &session_id,
        "You are a test assistant.",
        &provider,
        &tools,
        &tool_defs,
        None,
        koda_core::approval::ApprovalMode::Auto,
        &mut settings,
        &sink,
        cancel,
        &mut cmd_rx,
    )
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
