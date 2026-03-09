//! TUI shared context — the mutable state struct for the event loop.
//!
//! Holds all mutable locals that were previously captured in `run()`'s
//! closure scope. Methods on this struct replace inline blocks.
//! See #209.

use crate::tui_history;
use crate::tui_render::TuiRenderer;
use crate::tui_types::{
    MIN_VIEWPORT_HEIGHT, MenuContent, PromptMode, ProviderWizard, Term, TuiState,
};
use crate::tui_viewport::{
    draw_viewport, emit_above, init_terminal, maybe_resize_viewport, restore_terminal,
};

use anyhow::Result;
use crossterm::event::EventStream;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    style::{Color, Modifier, Style},
    text::Line,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tui_textarea::TextArea;

/// All mutable TUI state, extracted from `run()`'s local variables.
///
/// # State groups
///
/// TODO: once stable, consider splitting into `TuiUiState`
/// (terminal, textarea, renderer, menu, prompt_mode, viewport_height)
/// and `TuiSessionState` (config, provider, session, agent, db).
/// For now, a single struct is the pragmatic first extraction.
pub(crate) struct TuiContext {
    // ── UI state ─────────────────────────────────────────────
    pub terminal: Term,
    pub textarea: TextArea<'static>,
    pub renderer: TuiRenderer,
    pub viewport_height: u16,
    pub crossterm_events: EventStream,

    // ── Interaction state ─────────────────────────────────────
    pub tui_state: TuiState,
    pub menu: MenuContent,
    pub prompt_mode: PromptMode,
    pub provider_wizard: Option<ProviderWizard>,
    pub pending_approval_id: Option<String>,

    // ── Control flow ──────────────────────────────────────────
    pub input_queue: VecDeque<String>,
    pub pending_command: Option<String>,
    pub should_quit: bool,
    pub silent_compact_deferred: bool,
    pub inference_start: Option<std::time::Instant>,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub completer: crate::completer::InputCompleter,

    // ── Session state (shared references) ────────────────────
    // Lock discipline for `provider: Arc<RwLock<_>>`:
    // - Methods that swap the provider (handle_command) acquire write lock.
    //   Must NOT hold across .await points.
    // - Methods that read model info acquire read lock briefly.
    // - Sequential dispatch in run() prevents concurrent access.
    // Rule: acquire lock, do sync work, drop lock, then .await.
    pub config: KodaConfig,
    pub provider: Arc<RwLock<Box<dyn LlmProvider>>>,
    pub session: KodaSession,
    pub shared_mode: approval::SharedMode,
    pub agent: Arc<KodaAgent>,
    pub project_root: PathBuf,
}

impl TuiContext {
    /// Initialize all TUI state. Call before entering the event loop.
    ///
    /// This handles provider setup, auto-detection, terminal init,
    /// onboarding, and everything that `run()` used to do before `loop {`.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        project_root: PathBuf,
        mut config: KodaConfig,
        db: koda_core::db::Database,
        session_id: String,
        version_check: tokio::task::JoinHandle<Option<String>>,
        first_run: bool,
    ) -> Result<Self> {
        // Restore last-used provider
        let settings = koda_core::approval::Settings::load();
        if let Some(ref last) = settings.last_provider {
            let ptype =
                koda_core::config::ProviderType::from_url_or_name("", Some(&last.provider_type));
            config.provider_type = ptype;
            config.base_url = last.base_url.clone();
            config.model = last.model.clone();
            config.model_settings.model = last.model.clone();
            config.recalculate_model_derived();
        }

        let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
            Arc::new(RwLock::new(koda_core::providers::create_provider(&config)));

        if config.model == "auto-detect" {
            let prov = provider.read().await;
            match prov.list_models().await {
                Ok(models) if !models.is_empty() => {
                    config.model = models[0].id.clone();
                    config.model_settings.model = config.model.clone();
                    config.recalculate_model_derived();
                    tracing::info!("Auto-detected model: {}", config.model);
                }
                Ok(_) => {
                    config.model = "(no model loaded)".to_string();
                    config.model_settings.model = config.model.clone();
                }
                Err(e) => {
                    config.model = "(connection failed)".to_string();
                    config.model_settings.model = config.model.clone();
                    tracing::warn!("Auto-detect failed: {e}");
                }
            }
        }

        if config.model != "(no model loaded)" && config.model != "(connection failed)" {
            let prov = provider.read().await;
            config.query_and_apply_capabilities(prov.as_ref()).await;
        }

        // Print startup UI BEFORE entering raw mode
        let recent = db.recent_user_messages(3).await.unwrap_or_default();
        crate::startup::print_banner(&config, &recent);
        crate::startup::print_model_warning(&config);

        if let Ok(Some(latest)) = version_check.await
            && let Some((current, latest)) = koda_core::version::update_available(&latest)
        {
            crate::startup::print_update_notice(current, &latest);
        }

        let agent =
            Arc::new(koda_core::agent::KodaAgent::new(&config, project_root.clone()).await?);
        crate::startup::print_mcp_status(&agent.mcp_statuses);

        let session = KodaSession::new(session_id, agent.clone(), db, &config, ApprovalMode::Auto);

        let shared_mode = approval::new_shared_mode(ApprovalMode::Auto);

        // Terminal + textarea
        let viewport_height = MIN_VIEWPORT_HEIGHT;
        let terminal = init_terminal(viewport_height)?;

        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_cursor_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::REVERSED),
        );
        textarea.set_placeholder_text("Type a message...");
        textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));

        let mut renderer = TuiRenderer::new();
        renderer.model = config.model.clone();

        let mut completer = crate::completer::InputCompleter::new(project_root.clone());
        {
            let prov = provider.read().await;
            if let Ok(models) = prov.list_models().await {
                completer.set_model_names(models.iter().map(|m| m.id.clone()).collect());
            }
        }

        let mut menu = MenuContent::None;
        if first_run {
            // Onboarding: auto-open provider dropdown
            // (emit_above happens after terminal is created below)
            let providers = crate::repl::PROVIDERS;
            let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
                .iter()
                .map(
                    |(key, name, desc)| crate::widgets::provider_menu::ProviderItem {
                        key,
                        name,
                        description: desc,
                        is_current: false,
                    },
                )
                .collect();
            menu = MenuContent::Provider(crate::widgets::dropdown::DropdownState::new(
                items,
                "\u{1f43b} Choose your LLM provider",
            ));
        }

        Ok(Self {
            terminal,
            textarea,
            renderer,
            viewport_height,
            crossterm_events: EventStream::new(),
            tui_state: TuiState::Idle,
            menu,
            prompt_mode: PromptMode::Chat,
            provider_wizard: None,
            pending_approval_id: None,
            input_queue: VecDeque::new(),
            pending_command: None,
            should_quit: false,
            silent_compact_deferred: false,
            inference_start: None,
            history: tui_history::load_history(),
            history_idx: None,
            completer,
            config,
            provider,
            session,
            shared_mode,
            agent,
            project_root,
        })
    }

    /// Draw the viewport (resize if textarea grew/shrank).
    pub fn draw(&mut self) -> Result<()> {
        let mode = approval::read_mode(&self.shared_mode);
        let ctx = koda_core::context::percentage() as u32;

        maybe_resize_viewport(
            &mut self.terminal,
            &self.textarea,
            &mut self.viewport_height,
        )?;

        let config = &self.config;
        let textarea = &self.textarea;
        let tui_state = self.tui_state;
        let prompt_mode = &self.prompt_mode;
        let queue_len = self.input_queue.len();
        let elapsed = self
            .inference_start
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0);
        let last_turn = self.renderer.last_turn_stats.as_ref();
        let menu = &self.menu;

        self.terminal.draw(|f| {
            draw_viewport(
                f,
                textarea,
                &config.model,
                config.model_tier.label(),
                mode,
                ctx,
                tui_state,
                prompt_mode,
                queue_len,
                elapsed,
                last_turn,
                menu,
            );
        })?;
        Ok(())
    }

    /// Write a message line above the viewport.
    pub fn emit(&mut self, line: Line<'_>) {
        emit_above(&mut self.terminal, line);
    }

    /// Clean up terminal and print exit info.
    pub async fn cleanup(&mut self) {
        restore_terminal(&mut self.terminal, self.viewport_height);
        {
            let mut mcp = self.agent.mcp_registry.write().await;
            mcp.shutdown();
        }
        crate::startup::print_resume_hint(&self.session.id);
    }
}
