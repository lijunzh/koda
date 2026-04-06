//! Shared test harness for E2E tests.
//!
//! Provides `Env` — an isolated test environment with temp dir, DB,
//! session, config, and tool registry.  Every E2E test file imports this.

use koda_core::persistence::Persistence;
use koda_core::{
    approval::ApprovalMode,
    config::{KodaConfig, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent, sink::TestSink},
    inference::{self, InferenceContext},
    providers::{LlmProvider, mock::MockProvider},
    tools::ToolRegistry,
};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mutex to serialize tests that share process-global env vars
/// (KODA_MOCK_RESPONSES). `#[tokio::test]` runs tests concurrently
/// within the same process, so unsynchronized set_var/remove_var
/// on the same env var is a data race.
#[allow(dead_code)] // Only used by e2e_agent_test, but compiled into every test binary.
pub static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct Env {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub db: Database,
    pub session_id: String,
    pub config: KodaConfig,
    pub tools: ToolRegistry,
}

impl Env {
    pub async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = Database::init(&root).await.unwrap();
        let session_id = db.create_session("test-agent", &root).await.unwrap();
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let tools = ToolRegistry::new(root.clone(), config.max_context_tokens);
        Self {
            _tmp: tmp,
            root,
            db,
            session_id,
            config,
            tools,
        }
    }

    pub fn tool_defs(&self) -> Vec<koda_core::providers::ToolDefinition> {
        self.tools.get_definitions(&[], &[])
    }

    pub async fn insert_user_message(&self, text: &str) {
        self.db
            .insert_message(&self.session_id, &Role::User, Some(text), None, None, None)
            .await
            .unwrap();
    }

    /// Run inference with a MockProvider (convenience wrapper, asserts success).
    pub async fn run_inference(&self, provider: &MockProvider) -> Vec<EngineEvent> {
        self.run_inference_dyn(provider).await
    }

    /// Run inference with any LlmProvider, asserts success.
    pub async fn run_inference_dyn(&self, provider: &dyn LlmProvider) -> Vec<EngineEvent> {
        let (result, events) = self.run_inference_result(provider).await;
        assert!(result.is_ok(), "inference_loop failed: {:?}", result.err());
        events
    }

    /// Run inference and return Result + events (for testing error paths).
    pub async fn run_inference_result(
        &self,
        provider: &dyn LlmProvider,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>) {
        self.run_inference_full(provider, CancellationToken::new())
            .await
    }

    /// Run inference with a cancellation token.
    pub async fn run_inference_cancellable(
        &self,
        provider: &dyn LlmProvider,
        cancel: CancellationToken,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>) {
        self.run_inference_full(provider, cancel).await
    }

    /// Full inference run with all knobs exposed.
    async fn run_inference_full(
        &self,
        provider: &dyn LlmProvider,
        cancel: CancellationToken,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>) {
        let sink = TestSink::new();
        let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
        let tool_defs = self.tool_defs();
        let mut file_tracker =
            koda_core::file_tracker::FileTracker::new(&self.session_id, self.db.clone()).await;

        let result = inference::inference_loop(InferenceContext {
            project_root: &self.root,
            config: &self.config,
            db: &self.db,
            session_id: &self.session_id,
            system_prompt: "You are a test assistant.",
            provider,
            tools: &self.tools,
            tool_defs: &tool_defs,
            pending_images: None,
            mode: ApprovalMode::Auto,
            sink: &sink,
            cancel,
            cmd_rx: &mut cmd_rx,
            file_tracker: &mut file_tracker,
        })
        .await;

        (result, sink.events())
    }
}
