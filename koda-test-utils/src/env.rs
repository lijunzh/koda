//! Shared test environment for E2E tests.
//!
//! Provides [`Env`] — an isolated test environment with temp dir, DB,
//! session, config, and tool registry.

use koda_core::persistence::Persistence;
use koda_core::{
    config::{KodaConfig, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent, sink::TestSink},
    inference::{self, InferenceContext},
    providers::{LlmProvider, mock::MockProvider},
    tools::ToolRegistry,
    trust::TrustMode,
};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mutex to serialize tests that share process-global env vars
/// (KODA_MOCK_RESPONSES).  `#[tokio::test]` runs tests concurrently
/// within the same process, so unsynchronized set_var/remove_var
/// on the same env var is a data race.
pub static ENV_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An isolated test environment with temp dir, DB, session, config,
/// and tool registry.
pub struct Env {
    /// The temporary directory backing this environment.
    /// Kept alive for the lifetime of `Env`.
    pub _tmp: tempfile::TempDir,
    /// Path to the project root (inside `_tmp`).
    pub root: PathBuf,
    /// SQLite database for this test.
    pub db: Database,
    /// The test session ID.
    pub session_id: String,
    /// Koda configuration for this test.
    pub config: KodaConfig,
    /// Tool registry scoped to the test root.
    pub tools: ToolRegistry,
    /// Trust mode used by the inference loop (default: `Auto`).
    pub trust: TrustMode,
}

/// Builder for [`Env`] — customise provider, context window, agent name, etc.
///
/// ```rust,ignore
/// let env = Env::builder()
///     .max_context_tokens(100_000)
///     .provider_type(ProviderType::Mock)
///     .build()
///     .await;
/// ```
pub struct EnvBuilder {
    provider_type: ProviderType,
    agent_name: String,
    max_context_tokens: Option<usize>,
    trust: TrustMode,
}

impl Default for EnvBuilder {
    fn default() -> Self {
        Self {
            provider_type: ProviderType::LMStudio,
            agent_name: "test-agent".into(),
            max_context_tokens: None,
            trust: TrustMode::Auto,
        }
    }
}

impl EnvBuilder {
    /// Override the provider type (default: `LMStudio`).
    pub fn provider_type(mut self, p: ProviderType) -> Self {
        self.provider_type = p;
        self
    }

    /// Override the agent name stored in the session (default: `"test-agent"`).
    pub fn agent_name(mut self, name: impl Into<String>) -> Self {
        self.agent_name = name.into();
        self
    }

    /// Override `max_context_tokens` on the config (default: provider default).
    pub fn max_context_tokens(mut self, n: usize) -> Self {
        self.max_context_tokens = Some(n);
        self
    }

    /// Override the trust mode used by the inference loop (default: `Auto`).
    /// Sets both the inference-loop `mode` and the config trust so
    /// sub-agent clamping observes the correct parent value.
    pub fn trust(mut self, trust: TrustMode) -> Self {
        self.trust = trust;
        self
    }

    /// Build the environment.  Creates a temp dir, DB, session, and tool registry.
    pub async fn build(self) -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db = Database::init(&root).await.unwrap();
        let session_id = db.create_session(&self.agent_name, &root).await.unwrap();
        let mut config = KodaConfig::default_for_testing(self.provider_type);
        config.trust = self.trust;
        if let Some(n) = self.max_context_tokens {
            config.max_context_tokens = n;
            config.model_settings.max_context_tokens = n;
        }
        let tools = ToolRegistry::with_trust(root.clone(), config.max_context_tokens, self.trust);
        Env {
            _tmp: tmp,
            root,
            db,
            session_id,
            config,
            tools,
            trust: self.trust,
        }
    }
}

impl Env {
    /// Create a fresh, isolated test environment with defaults.
    pub async fn new() -> Self {
        Self::builder().build().await
    }

    /// Start building a customised environment.
    pub fn builder() -> EnvBuilder {
        EnvBuilder::default()
    }

    /// Get tool definitions (no disabled/allowed filters).
    pub fn tool_defs(&self) -> Vec<koda_core::providers::ToolDefinition> {
        self.tools.get_definitions(&[], &[])
    }

    /// Insert a user message into the test session.
    pub async fn insert_user_message(&self, text: &str) {
        self.insert_message(&Role::User, text).await;
    }

    /// Insert a message with any role into the test session.
    /// Assistant messages are immediately marked complete — they represent
    /// finished turns, not in-progress streams. Tests that want to simulate
    /// an interrupted turn should call `db.insert_message` directly and
    /// omit `mark_message_complete`.
    pub async fn insert_message(&self, role: &Role, text: &str) {
        let mid = self
            .db
            .insert_message(&self.session_id, role, Some(text), None, None, None)
            .await
            .unwrap();
        if *role == Role::Assistant {
            self.db.mark_message_complete(mid).await.unwrap();
        }
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
            mode: self.trust,
            sink: &sink,
            cancel,
            cmd_rx: &mut cmd_rx,
            file_tracker: &mut file_tracker,
        })
        .await;

        (result, sink.events())
    }
}
