//! KodaSession — per-conversation state.
//!
//! Holds mutable, per-turn state: database handle, session ID,
//! provider instance, approval mode, and cancellation token.
//! Instantiable N times for parallel sub-agents or cowork mode.
//!
//! ## Architecture
//!
//! ```text
//! KodaAgent (shared, immutable)
//!   ├─ tools, system prompt, project root
//!   └─ shared via Arc across sessions
//!
//! KodaSession (per-conversation, mutable)
//!   ├─ database handle (SQLite)
//!   ├─ session_id (UUID)
//!   ├─ provider instance
//!   ├─ trust mode (plan/safe/auto)
//!   └─ cancellation token
//! ```
//!
//! This split allows the same agent to power multiple concurrent sessions
//! (e.g., main REPL + background sub-agents) without shared mutable state.

use crate::agent::KodaAgent;
use crate::config::KodaConfig;
use crate::db::Database;
use crate::engine::{EngineCommand, EngineSink};
use crate::file_tracker::FileTracker;
use crate::inference::InferenceContext;
use crate::providers::{self, ImageData, LlmProvider};
use crate::trust::TrustMode;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A single conversation session with its own state.
///
/// Each session has its own provider, trust mode, and cancel token.
/// Multiple sessions can share the same `Arc<KodaAgent>`.
pub struct KodaSession {
    /// Unique session identifier.
    pub id: String,
    /// Shared agent configuration (tools, system prompt).
    pub agent: Arc<KodaAgent>,
    /// Database handle for message persistence.
    pub db: Database,
    /// LLM provider for this session.
    pub provider: Box<dyn LlmProvider>,
    /// Current trust mode (Plan / Safe / Auto).
    pub mode: TrustMode,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// File lifecycle tracker — tracks files created by Koda (#465).
    pub file_tracker: FileTracker,
    /// Whether the session title has already been set (first-message guard).
    pub title_set: bool,
}

impl KodaSession {
    /// Create a new session from an agent, config, and database.
    pub async fn new(
        id: String,
        agent: Arc<KodaAgent>,
        db: Database,
        config: &KodaConfig,
        mode: TrustMode,
    ) -> Self {
        let provider = providers::create_provider(config);
        // Wire db+session into ToolRegistry for RecallContext
        agent.tools.set_session(Arc::new(db.clone()), id.clone());

        // Start MCP servers from DB config (#662)
        // TODO(#662 Phase 2): Move MCP manager to app-level ownership so
        // servers are shared across sessions and not duplicated on resume.
        match crate::mcp::McpManager::start_from_db(&db).await {
            Ok(manager) => {
                if !manager.is_empty() {
                    let mgr = Arc::new(tokio::sync::RwLock::new(manager));
                    agent.tools.set_mcp_manager(mgr);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to start MCP servers (non-fatal)");
            }
        }
        let file_tracker = FileTracker::new(&id, db.clone()).await;
        Self {
            id,
            agent,
            db,
            provider,
            mode,
            cancel: CancellationToken::new(),
            file_tracker,
            title_set: false,
        }
    }

    /// Run one inference turn: prompt → streaming → tool execution → response.
    ///
    /// Emits `TurnStart` and `TurnEnd` lifecycle events. The loop-cap prompt is handled via `EngineEvent::LoopCapReached` / `EngineCommand::LoopDecision`
    /// through the `cmd_rx` channel.
    pub async fn run_turn(
        &mut self,
        config: &KodaConfig,
        pending_images: Option<Vec<ImageData>>,
        sink: &dyn EngineSink,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> Result<()> {
        let turn_id = uuid::Uuid::new_v4().to_string();
        sink.emit(crate::engine::EngineEvent::TurnStart {
            turn_id: turn_id.clone(),
        });

        // Compose the per-turn system prompt: static `agent.system_prompt`
        // plus a dynamically-rendered MCP server-instructions section. We
        // do this per-turn (not at agent build time) because MCP servers
        // attach inside `KodaSession::new`, AFTER the static prompt is
        // built and the agent is wrapped in `Arc`. Composing here picks up
        // both the initial-connect case and any mid-session `/mcp add`
        // hot-reloads automatically (#922).
        let mcp_section = if let Some(mgr) = self.agent.tools.mcp_manager() {
            // Bind the Arc to extend its lifetime past the read guard
            // (try_read() returns a guard that borrows the lock).
            match mgr.try_read() {
                Ok(guard) => {
                    crate::prompt::render_mcp_instructions_section(&guard.server_instructions())
                }
                Err(_) => String::new(), // manager momentarily locked; skip this turn
            }
        } else {
            String::new()
        };
        let system_prompt = if mcp_section.is_empty() {
            self.agent.system_prompt.clone()
        } else {
            format!("{}{mcp_section}", self.agent.system_prompt)
        };

        let result = crate::inference::inference_loop(InferenceContext {
            project_root: &self.agent.project_root,
            config,
            db: &self.db,
            session_id: &self.id,
            system_prompt: &system_prompt,
            provider: self.provider.as_ref(),
            tools: &self.agent.tools,
            tool_defs: &self.agent.tool_defs,
            pending_images,
            mode: self.mode,
            sink,
            cancel: self.cancel.clone(),
            cmd_rx,
            file_tracker: &mut self.file_tracker,
        })
        .await;

        let reason = match &result {
            Ok(()) if self.cancel.is_cancelled() => crate::engine::event::TurnEndReason::Cancelled,
            Ok(()) => crate::engine::event::TurnEndReason::Complete,
            Err(e) => crate::engine::event::TurnEndReason::Error {
                message: e.to_string(),
            },
        };
        sink.emit(crate::engine::EngineEvent::TurnEnd { turn_id, reason });

        result
    }

    /// Replace the provider (e.g., after switching models or providers).
    pub fn update_provider(&mut self, config: &KodaConfig) {
        self.provider = providers::create_provider(config);
    }
}
