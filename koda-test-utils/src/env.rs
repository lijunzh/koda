//! Shared test environment for E2E tests.
//!
//! Provides [`Env`] — an isolated test environment with temp dir, DB,
//! session, config, and tool registry.

use koda_core::persistence::Persistence;
use koda_core::{
    child_agent::ChildAgentRegistry,
    config::{KodaConfig, ProviderType},
    db::{Database, Role},
    engine::{EngineCommand, EngineEvent, sink::TestSink},
    inference::{self, InferenceContext},
    providers::{LlmProvider, mock::MockProvider},
    tools::ToolRegistry,
    trust::TrustMode,
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Mutex to serialize tests that share process-global env vars
/// (KODA_MOCK_RESPONSES).  `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` runs tests concurrently
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
    /// Background agent registry shared across inference calls.
    ///
    /// Tests that spawn background sub-agents can poll this for
    /// live status snapshots, or call [`ChildAgentRegistry::subscribe`]
    /// to get a watch receiver for a specific task.
    pub bg_agents: Arc<ChildAgentRegistry>,
    /// Root agent's mailbox receiver. Held to keep the underlying
    /// `mpsc::Sender` half (inside `Mailbox`) alive — without this,
    /// dropping the receiver would close the channel and the
    /// bg-agent's `notify_parent_mailbox` send would silently no-op.
    /// (`watch::Sender::send_replace` would still bump the seq, so
    /// `WaitForMail` would unblock — but mail-bearing tests inspecting
    /// the queue would lose data. Keep it alive defensively.)
    pub _mailbox_rx: Arc<tokio::sync::Mutex<koda_core::agent::MailboxReceiver>>,
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

        // #1325 Phase 5b follow-up: tests that exercise WaitForMail /
        // SpawnAgent need a mailbox registry attached to the tool
        // registry, otherwise WaitForMail returns the "no registry"
        // error path and tests racing the bg-agent's `cache.put` flake
        // (the test thinks it's blocking until mail arrives, but it's
        // actually returning instantly with an error). Mirrors
        // `KodaSession::new` — fresh registry, root mailbox registered.
        let (mailbox, mailbox_rx) = koda_core::agent::Mailbox::new();
        let mailbox = Arc::new(mailbox);
        let mailbox_registry = Arc::new(koda_core::agent::mailbox_registry::MailboxRegistry::new());
        let _ = mailbox_registry.register(
            koda_core::agent::path::AgentPath::root(),
            Arc::clone(&mailbox),
        );
        tools.set_mailbox_registry(Arc::clone(&mailbox_registry));

        // #1338 Issue #3: mirror `KodaSession::new` — hand the
        // bg-agent registry to the tool layer so `WaitForMail`'s
        // timeout payload sees in-flight bg-agents in tests too.
        let bg_agents = koda_core::child_agent::new_shared();
        tools.set_bg_agents(Arc::clone(&bg_agents));

        Env {
            _tmp: tmp,
            root,
            db,
            session_id,
            config,
            tools,
            trust: self.trust,
            bg_agents,
            _mailbox_rx: Arc::new(tokio::sync::Mutex::new(mailbox_rx)),
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

    /// Run inference and cancel as soon as an event matching `pred` is
    /// emitted by the engine.
    ///
    /// **#1109 F3**: replaces `tokio::spawn(async { sleep(N).await; cancel(); })`
    /// patterns. Cancellation fires the moment the synchronization
    /// point of interest (e.g. `ToolCallStart`) is observed, making
    /// the test deterministic regardless of CI runner speed.
    pub async fn run_inference_cancel_on_event<F>(
        &self,
        provider: &dyn LlmProvider,
        pred: F,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>)
    where
        F: Fn(&EngineEvent) -> bool + Send + Sync + 'static,
    {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        // Build the sink up-front so we can subscribe before inference
        // starts — otherwise the predicate could miss the event we're
        // waiting for.
        let sink = Arc::new(TestSink::new());
        let mut rx = sink.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if pred(&ev) => {
                        cancel_clone.cancel();
                        break;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.run_inference_with_sink(provider, cancel, sink).await
    }

    /// Full inference run with all knobs exposed.
    async fn run_inference_full(
        &self,
        provider: &dyn LlmProvider,
        cancel: CancellationToken,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>) {
        self.run_inference_with_sink(provider, cancel, Arc::new(TestSink::new()))
            .await
    }

    /// Same as `run_inference_full` but lets the caller supply the
    /// sink — used by `run_inference_cancel_on_event` which needs to
    /// subscribe to the live event stream before inference begins.
    ///
    /// **Public so tests for asynchronously-spawned work (e.g.
    /// background sub-agents) can subscribe to the sink BEFORE
    /// inference starts and keep reading after it returns** — events
    /// emitted by tokio::spawn'd tasks may arrive after the parent's
    /// inference_loop completes, so the static `events()` snapshot at
    /// return time is not canonical for those tests. See the QA-001
    /// regression in `e2e_agent_test.rs` for the worked example.
    pub async fn run_inference_with_sink(
        &self,
        provider: &dyn LlmProvider,
        cancel: CancellationToken,
        sink: Arc<TestSink>,
    ) -> (anyhow::Result<()>, Vec<EngineEvent>) {
        let (_, mut cmd_rx) = mpsc::channel::<EngineCommand>(1);
        let tool_defs = self.tool_defs();
        let mut file_tracker =
            koda_core::file_tracker::FileTracker::new(&self.session_id, self.db.clone()).await;
        // #1022 B12: registry lives on the session in production.
        // Env now holds one shared registry per test so background
        // agents spawned during inference remain visible after
        // run_inference returns (enables QA-001 iteration-counter test).
        let bg_agents = Arc::clone(&self.bg_agents);
        let sub_agent_cache = koda_core::sub_agent_cache::SubAgentCache::new();

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
            sink: sink.as_ref(),
            cancel,
            cmd_rx: &mut cmd_rx,
            file_tracker: &mut file_tracker,
            bg_agents: &bg_agents,
            sub_agent_cache: &sub_agent_cache,
            // #1325 Phase 4: test envs are always the user-facing
            // root agent. SpawnAgent-driven children get their own
            // path injected by the spawn machinery and don't reach
            // here.
            agent_path: &koda_core::agent::AgentPath::root(),
        })
        .await;

        (result, sink.events())
    }

    /// Collect every `ChildTaskUpdate` event a background sub-agent will
    /// emit, regardless of which side of the parent's `inference_loop`
    /// drained them.
    ///
    /// **Why this helper exists** (#1109, PR #1113):
    ///
    /// `ChildTaskUpdate` events flow through a single-consumer queue
    /// inside [`ChildAgentRegistry`]. The parent's `inference_loop`
    /// drains them on every iteration and forwards to the sink. A
    /// test that asserts on these events is racing the parent for
    /// the same queue:
    ///
    /// * If the bg task **finishes before `run_inference` returns**,
    ///   the parent drained everything into the sink — the events
    ///   live in the `events` vec, and the registry queue is empty.
    /// * If the bg task **finishes after `run_inference` returns**,
    ///   the parent never got a chance — the events are sitting in
    ///   the registry queue waiting for `drain_status_events()`.
    ///
    /// Which side wins is non-deterministic (depends on tokio worker
    /// scheduling, OS, CI runner load). This helper merges both
    /// sources so callers don't have to know.
    ///
    /// # Arguments
    ///
    /// * `events_from_run_inference` — the `Vec<EngineEvent>`
    ///   returned by [`Self::run_inference`] (or any of its
    ///   variants). `ChildTaskUpdate` events are filtered out; other
    ///   event types are ignored.
    /// * `terminal_timeout` — how long to keep polling
    ///   [`ChildAgentRegistry::drain_status_events`] waiting for a
    ///   terminal status (`Completed`, `Errored`, `Cancelled`). Use
    ///   a generous bound (e.g. 10s) — on a healthy machine the
    ///   helper returns in single-digit milliseconds.
    ///
    /// # Returns
    ///
    /// `Ok(events)` if a terminal `ChildTaskUpdate` was observed within
    /// `terminal_timeout`, where `events` contains every
    /// `ChildTaskUpdate` from both sources in arrival order (sink-side
    /// events first, then queue-drained events).
    ///
    /// `Err(events)` if the timeout elapsed without a terminal
    /// status. The partial event list is returned for diagnostics.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let events = env.run_inference(&provider).await;
    /// let bg_events = env
    ///     .collect_bg_events_after(events, Duration::from_secs(10))
    ///     .await
    ///     .expect("bg task never reached terminal state");
    /// assert!(bg_events.iter().any(|e| matches!(
    ///     e, EngineEvent::ChildTaskUpdate {
    ///         status: AgentStatus::Completed { .. }, ..
    ///     }
    /// )));
    /// ```
    pub async fn collect_bg_events_after(
        &self,
        events_from_run_inference: Vec<EngineEvent>,
        terminal_timeout: std::time::Duration,
    ) -> Result<Vec<EngineEvent>, Vec<EngineEvent>> {
        let mut bg_events: Vec<EngineEvent> = events_from_run_inference
            .into_iter()
            .filter(|ev| matches!(ev, EngineEvent::ChildTaskUpdate { .. }))
            .collect();

        let deadline = tokio::time::Instant::now() + terminal_timeout;
        loop {
            for ev in self.bg_agents.drain_status_events() {
                bg_events.push(ev);
            }
            if bg_events.iter().any(is_terminal_bg_update) {
                return Ok(bg_events);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(bg_events);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Returns `true` if `ev` is a [`EngineEvent::ChildTaskUpdate`] in a
/// terminal status (`Completed`, `Errored`, or `Cancelled`).
///
/// Extracted as a free function so [`Env::collect_bg_events_after`]
/// stays readable; the matcher itself is non-trivial because
/// `AgentStatus` has additional non-terminal variants we must not
/// mistakenly treat as final.
fn is_terminal_bg_update(ev: &EngineEvent) -> bool {
    use koda_core::child_agent::AgentStatus;
    matches!(
        ev,
        EngineEvent::ChildTaskUpdate {
            status: AgentStatus::Completed { .. }
                | AgentStatus::Errored { .. }
                | AgentStatus::Cancelled,
            ..
        }
    )
}
