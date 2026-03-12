//! KodaSession — per-conversation state.
//!
//! Holds mutable, per-turn state: database handle, session ID,
//! provider instance, approval settings, and cancellation token.
//! Instantiable N times for parallel sub-agents or cowork mode.

use crate::agent::KodaAgent;
use crate::approval::ApprovalMode;
use crate::config::KodaConfig;
use crate::db::Database;
use crate::engine::{EngineCommand, EngineSink};
use crate::inference::InferenceContext;
use crate::providers::{self, ImageData, LlmProvider};
use crate::settings::Settings;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A single conversation session with its own state.
///
/// Each session has its own provider, approval settings, and cancel token.
/// Multiple sessions can share the same `Arc<KodaAgent>`.
pub struct KodaSession {
    pub id: String,
    pub agent: Arc<KodaAgent>,
    pub db: Database,
    pub provider: Box<dyn LlmProvider>,
    pub mode: ApprovalMode,
    pub settings: Settings,
    pub cancel: CancellationToken,
}

impl KodaSession {
    /// Create a new session from an agent, config, and database.
    pub fn new(
        id: String,
        agent: Arc<KodaAgent>,
        db: Database,
        config: &KodaConfig,
        mode: ApprovalMode,
    ) -> Self {
        let provider = providers::create_provider(config);
        let settings = Settings::load();
        // Wire db+session into ToolRegistry for RecallContext
        agent.tools.set_session(Arc::new(db.clone()), id.clone());
        Self {
            id,
            agent,
            db,
            provider,
            mode,
            settings,
            cancel: CancellationToken::new(),
        }
    }

    /// Run one inference turn: prompt → streaming → tool execution → response.
    ///
    /// Emits `TurnStart` and `TurnEnd` lifecycle events. The loop-cap prompt
    /// is handled via `EngineEvent::LoopCapReached` / `EngineCommand::LoopDecision`
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

        let result = crate::inference::inference_loop(InferenceContext {
            project_root: &self.agent.project_root,
            config,
            db: &self.db,
            session_id: &self.id,
            system_prompt: &self.agent.system_prompt,
            provider: self.provider.as_ref(),
            tools: &self.agent.tools,
            tool_defs: &self.agent.tool_defs,
            pending_images,
            mode: self.mode,
            settings: &mut self.settings,
            sink,
            cancel: self.cancel.clone(),
            cmd_rx,
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
