//! TUI shared context — the mutable state struct for the event loop.
//!
//! Holds all mutable locals that were previously captured in `run()`'s
//! closure scope. Methods on this struct replace inline blocks.
//! See #209.

use crate::input;
use crate::sink::UiEvent;
use crate::tui_commands::{self, SlashAction};
use crate::tui_history;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use crate::tui_types::{
    MIN_VIEWPORT_HEIGHT, MenuContent, PromptMode, ProviderWizard, Term, TuiState,
};
use crate::tui_viewport::{
    draw_viewport, emit_above, init_terminal, maybe_resize_viewport, reinit_viewport_in_place,
    restore_terminal,
};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::Role;
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::persistence::Persistence;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
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
        skip_probe: bool,
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

        let mut session =
            KodaSession::new(session_id, agent.clone(), db, &config, ApprovalMode::Auto);
        session.skip_probe = skip_probe;

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
                "koda",
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

    /// The main event loop. Dispatches queued commands, handles inference
    /// turns, and processes idle keyboard input.
    ///
    /// Channels stay in the caller (`run()`) and are passed in because
    /// they're consumed differently by `tokio::select!`.
    #[allow(clippy::too_many_lines)]
    pub async fn run_event_loop(
        &mut self,
        ui_tx: &mpsc::Sender<UiEvent>,
        ui_rx: &mut mpsc::Receiver<UiEvent>,
        cmd_tx: &mpsc::Sender<EngineCommand>,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> Result<()> {
        // ── Main event loop ──────────────────────────────────────

        loop {
            if self.should_quit {
                break;
            }

            // Check if we have a queued or pending command to process
            if self.tui_state == TuiState::Idle {
                let input = if let Some(cmd) = self.pending_command.take() {
                    Some(cmd)
                } else if let Some(queued) = self.input_queue.pop_front() {
                    // Echo queued input above viewport
                    let mode = approval::read_mode(&self.shared_mode);
                    let icon = match mode {
                        ApprovalMode::Safe => "🔍",
                        ApprovalMode::Strict => "🔒",
                        ApprovalMode::Auto => "⚡",
                    };
                    emit_above(
                        &mut self.terminal,
                        Line::from(vec![
                            Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                            Span::raw(queued.clone()),
                        ]),
                    );
                    Some(queued)
                } else {
                    None
                };

                if let Some(input) = input {
                    let input = input.trim().to_string();
                    if !input.is_empty() {
                        // Try slash commands first
                        if input.starts_with('/') {
                            // Intercept /model (no args) — open inline dropdown.
                            if input.trim() == "/model" {
                                let prov = self.provider.read().await;
                                match prov.list_models().await {
                                    Ok(models) if !models.is_empty() => {
                                        let items: Vec<crate::widgets::model_menu::ModelItem> =
                                            models
                                                .iter()
                                                .map(|m| crate::widgets::model_menu::ModelItem {
                                                    id: m.id.clone(),
                                                    is_current: m.id == self.config.model,
                                                })
                                                .collect();
                                        let mut dd = crate::widgets::dropdown::DropdownState::new(
                                            items,
                                            "\u{1f43b} Select a model",
                                        );
                                        // Pre-select current model
                                        if let Some(idx) =
                                            dd.filtered.iter().position(|m| m.is_current)
                                        {
                                            dd.selected = idx;
                                            // Adjust scroll so current model is visible
                                            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                            if idx >= max_vis {
                                                dd.scroll_offset = idx + 1 - max_vis;
                                            }
                                        }
                                        self.menu = MenuContent::Model(dd);
                                    }
                                    Ok(_) => {
                                        emit_above(
                                            &mut self.terminal,
                                            Line::styled(
                                                "  \u{26a0} No models available",
                                                Style::default().fg(Color::Yellow),
                                            ),
                                        );
                                    }
                                    Err(e) => {
                                        emit_above(
                                            &mut self.terminal,
                                            Line::styled(
                                                format!("  \u{2717} Failed to list models: {e}"),
                                                Style::default().fg(Color::Red),
                                            ),
                                        );
                                    }
                                }
                                continue;
                            }

                            // Intercept /self.provider (no args) — open inline dropdown
                            if input.trim() == "/provider" {
                                let providers = crate::repl::PROVIDERS;
                                let items: Vec<crate::widgets::provider_menu::ProviderItem> =
                                    providers
                                        .iter()
                                        .map(|(key, name, desc)| {
                                            crate::widgets::provider_menu::ProviderItem {
                                        key,
                                        name,
                                        description: desc,
                                        is_current:
                                            koda_core::config::ProviderType::from_url_or_name(
                                                "",
                                                Some(key),
                                            ) == self.config.provider_type,
                                    }
                                        })
                                        .collect();
                                let mut dd = crate::widgets::dropdown::DropdownState::new(
                                    items,
                                    "\u{1f43b} Select a provider",
                                );
                                // Pre-select current self.provider
                                if let Some(idx) = dd.filtered.iter().position(|p| p.is_current) {
                                    dd.selected = idx;
                                    let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                    if idx >= max_vis {
                                        dd.scroll_offset = idx + 1 - max_vis;
                                    }
                                }
                                self.menu = MenuContent::Provider(dd);
                                continue;
                            }

                            // Intercept /self.provider <name> — skip dropdown, start wizard at API key step
                            if input.trim().starts_with("/provider ") {
                                let name = input.trim().strip_prefix("/provider ").unwrap().trim();
                                let ptype = koda_core::config::ProviderType::from_url_or_name(
                                    "",
                                    Some(name),
                                );
                                let base_url = ptype.default_base_url().to_string();
                                let provider_name = ptype.to_string();

                                if ptype.requires_api_key() {
                                    let env_name = ptype.env_key_name().to_string();
                                    // Check if key already exists in keystore
                                    if koda_core::runtime_env::is_set(&env_name) {
                                        // Key exists — just switch self.provider, no wizard
                                        self.config.provider_type = ptype.clone();
                                        self.config.base_url = base_url;
                                        self.config.model = ptype.default_model().to_string();
                                        self.config.model_settings.model =
                                            self.config.model.clone();
                                        self.config.recalculate_model_derived();
                                        *self.provider.write().await =
                                            koda_core::providers::create_provider(&self.config);
                                        crate::tui_wizards::save_provider(&self.config);
                                        let prov = self.provider.read().await;
                                        if let Ok(models) = prov.list_models().await {
                                            if let Some(first) = models.first() {
                                                self.config.model = first.id.clone();
                                                self.config.model_settings.model =
                                                    self.config.model.clone();
                                                self.config.recalculate_model_derived();
                                            }
                                            self.config
                                                .query_and_apply_capabilities(prov.as_ref())
                                                .await;
                                            self.completer.set_model_names(
                                                models.iter().map(|m| m.id.clone()).collect(),
                                            );
                                        }
                                        self.renderer.model = self.config.model.clone();
                                        emit_above(
                                            &mut self.terminal,
                                            Line::styled(
                                                format!(
                                                    "  \u{2714} Provider: {} ({})",
                                                    self.config.provider_type, self.config.model
                                                ),
                                                Style::default().fg(Color::Green),
                                            ),
                                        );
                                    } else {
                                        // Need API key — start wizard at step 2
                                        self.menu = MenuContent::WizardTrail(vec![(
                                            "Provider".into(),
                                            provider_name,
                                        )]);
                                        self.prompt_mode = PromptMode::WizardInput {
                                            label: format!("API key for {}", ptype),
                                            masked: true,
                                        };
                                        self.provider_wizard = Some(ProviderWizard::NeedApiKey {
                                            provider_type: ptype,
                                            base_url,
                                            env_name,
                                        });
                                        self.textarea.select_all();
                                        self.textarea.cut();
                                    }
                                } else {
                                    // Local self.provider — start wizard at URL step
                                    self.menu = MenuContent::WizardTrail(vec![(
                                        "Provider".into(),
                                        provider_name,
                                    )]);
                                    self.prompt_mode = PromptMode::WizardInput {
                                        label: format!("{} URL", ptype),
                                        masked: false,
                                    };
                                    self.provider_wizard = Some(ProviderWizard::NeedUrl {
                                        provider_type: ptype,
                                    });
                                    self.textarea.select_all();
                                    self.textarea.cut();
                                    self.textarea.insert_str(&base_url);
                                }
                                continue;
                            }

                            // Intercept /sessions (no args) — open inline dropdown
                            if input.trim() == "/sessions" {
                                match self.session.db.list_sessions(10, &self.project_root).await {
                                    Ok(sessions) if !sessions.is_empty() => {
                                        let items: Vec<crate::widgets::session_menu::SessionItem> =
                                            sessions
                                                .iter()
                                                .map(|s| {
                                                    crate::widgets::session_menu::SessionItem {
                                                        id: s.id.clone(),
                                                        short_id: s.id[..8.min(s.id.len())]
                                                            .to_string(),
                                                        created_at: s.created_at.clone(),
                                                        message_count: s.message_count,
                                                        total_tokens: s.total_tokens,
                                                        is_current: s.id == self.session.id,
                                                    }
                                                })
                                                .collect();
                                        let mut dd = crate::widgets::dropdown::DropdownState::new(
                                            items,
                                            "\u{1f43b} Sessions",
                                        );
                                        // Pre-select current self.session
                                        if let Some(idx) =
                                            dd.filtered.iter().position(|s| s.is_current)
                                        {
                                            dd.selected = idx;
                                            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                                            if idx >= max_vis {
                                                dd.scroll_offset = idx + 1 - max_vis;
                                            }
                                        }
                                        self.menu = MenuContent::Session(dd);
                                    }
                                    Ok(_) => {
                                        emit_above(
                                            &mut self.terminal,
                                            Line::styled(
                                                "  No other sessions found.",
                                                Style::default().fg(Color::DarkGray),
                                            ),
                                        );
                                    }
                                    Err(e) => {
                                        emit_above(
                                            &mut self.terminal,
                                            Line::styled(
                                                format!("  \u{2717} Error: {e}"),
                                                Style::default().fg(Color::Red),
                                            ),
                                        );
                                    }
                                }
                                continue;
                            }

                            let action = tui_commands::handle_slash_command(
                                &mut self.terminal,
                                &input,
                                &mut self.config,
                                &self.provider,
                                &mut self.session,
                                &self.shared_mode,
                                &mut self.renderer,
                                &self.project_root,
                                &self.agent,
                                &mut self.pending_command,
                            )
                            .await;

                            match action {
                                SlashAction::Continue => {
                                    // Re-init self.terminal to resync viewport with cursor
                                    // position after crossterm direct writes.
                                    self.viewport_height = MIN_VIEWPORT_HEIGHT;
                                    // Drop the old EventStream BEFORE init_terminal.
                                    // EventStream spawns a background wake thread that
                                    // reads from stdin; if it's still active it can
                                    // consume the DSR response that Viewport::Inline's
                                    // cursor-position query needs, causing a timeout.
                                    self.crossterm_events = EventStream::new();
                                    self.terminal = init_terminal(self.viewport_height)?;
                                    // Refresh model name cache (self.provider may have changed)
                                    let prov = self.provider.read().await;
                                    if let Ok(models) = prov.list_models().await {
                                        self.completer.set_model_names(
                                            models.iter().map(|m| m.id.clone()).collect(),
                                        );
                                    }
                                    // Sync model name for cost estimation
                                    self.renderer.model = self.config.model.clone();
                                    // Force immediate redraw so the prompt is visible
                                    // after slash command output (don't wait for next event).
                                    let mode = approval::read_mode(&self.shared_mode);
                                    let ctx = koda_core::context::percentage() as u32;
                                    self.terminal.draw(|f| {
                                        draw_viewport(
                                            f,
                                            &self.textarea,
                                            &self.config.model,
                                            "koda",
                                            mode,
                                            ctx,
                                            self.tui_state,
                                            &self.prompt_mode,
                                            self.input_queue.len(),
                                            0,
                                            self.renderer.last_turn_stats.as_ref(),
                                            &self.menu,
                                        );
                                    })?;
                                }
                                SlashAction::Quit => {
                                    tui_output::emit_line(
                                        &mut self.terminal,
                                        Line::styled(
                                            "\u{1f43b} Goodbye!",
                                            Style::default().fg(Color::Cyan),
                                        ),
                                    );
                                    self.should_quit = true;
                                    continue;
                                }
                            }
                        } else {
                            // ── Start inference turn inline ──────────
                            let user_input = input.clone();
                            let processed = input::process_input(&user_input, &self.project_root);
                            if !processed.images.is_empty() {
                                for (i, _img) in processed.images.iter().enumerate() {
                                    emit_above(
                                        &mut self.terminal,
                                        Line::from(vec![
                                            ratatui::text::Span::raw("  "),
                                            ratatui::text::Span::styled(
                                                format!("\u{1f5bc} Image {}", i + 1),
                                                Style::default().fg(Color::Magenta),
                                            ),
                                        ]),
                                    );
                                }
                            }

                            let user_message = if let Some(context) =
                                input::format_context_files(&processed.context_files)
                            {
                                for f in &processed.context_files {
                                    emit_above(
                                        &mut self.terminal,
                                        Line::from(vec![
                                            ratatui::text::Span::raw("  "),
                                            ratatui::text::Span::styled(
                                                format!("\u{1f4ce} {}", f.path),
                                                Style::default().fg(Color::Cyan),
                                            ),
                                        ]),
                                    );
                                }
                                format!("{}\n\n{context}", processed.prompt)
                            } else {
                                processed.prompt.clone()
                            };

                            if let Err(e) = self
                                .session
                                .db
                                .insert_message(
                                    &self.session.id,
                                    &Role::User,
                                    Some(&user_message),
                                    None,
                                    None,
                                    None,
                                )
                                .await
                            {
                                tracing::warn!("Failed to persist user message: {e}");
                            }

                            let pending_images = if processed.images.is_empty() {
                                None
                            } else {
                                Some(processed.images)
                            };

                            self.session.mode = approval::read_mode(&self.shared_mode);
                            self.session.update_provider(&self.config);

                            let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());
                            let cancel_token = self.session.cancel.clone();

                            // Run the inference turn as a pinned future
                            self.tui_state = TuiState::Inferring;
                            self.inference_start = Some(std::time::Instant::now());
                            self.renderer.last_turn_stats = None;

                            {
                                let turn = self.session.run_turn(
                                    &self.config,
                                    pending_images,
                                    &cli_sink,
                                    cmd_rx,
                                );
                                tokio::pin!(turn);

                                loop {
                                    // Redraw viewport inside inference loop
                                    let mode = approval::read_mode(&self.shared_mode);
                                    let ctx = koda_core::context::percentage() as u32;
                                    self.terminal.draw(|f| {
                                        draw_viewport(
                                            f,
                                            &self.textarea,
                                            &self.config.model,
                                            "koda",
                                            mode,
                                            ctx,
                                            self.tui_state,
                                            &self.prompt_mode,
                                            self.input_queue.len(),
                                            self.inference_start
                                                .map(|s| s.elapsed().as_secs())
                                                .unwrap_or(0),
                                            self.renderer.last_turn_stats.as_ref(),
                                            &self.menu,
                                        );
                                    })?;

                                    tokio::select! {
                                        result = &mut turn => {
                                            if let Err(e) = result {
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::from(vec![
                                                        ratatui::text::Span::raw("  "),
                                                        ratatui::text::Span::styled(
                                                            format!("\u{2717} Turn failed: {e:#}"),
                                                            Style::default().fg(Color::Red),
                                                        ),
                                                    ]),
                                                );
                                            }
                                            break;
                                        }
                                        Some(Ok(ev)) = self.crossterm_events.next() => {
                                            if let Event::Resize(_, _) = ev {
                                                // Terminal resized during inference — erase stale
                                                // viewport and reinit to prevent ghost prompt lines.
                                                reinit_viewport_in_place(&mut self.terminal, self.viewport_height, self.viewport_height)?;
                                            } else if let Event::Key(key) = ev {
                                                // Approval hotkeys during inference
                                                if let MenuContent::Approval { id, .. } = &self.menu {
                                                    let approval_id = id.clone();
                                                    let decision = match key.code {
                                                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                                                            Some(ApprovalDecision::Approve)
                                                        }
                                                        KeyCode::Char('n') | KeyCode::Char('N') => {
                                                            Some(ApprovalDecision::Reject)
                                                        }
                                                        KeyCode::Char('a') | KeyCode::Char('A') => {
                                                            // "Always allow" = approve + switch to Auto mode
                                                            approval::set_mode(&self.shared_mode, ApprovalMode::Auto);
                                                            Some(ApprovalDecision::Approve)
                                                        }
                                                        KeyCode::Char('f') | KeyCode::Char('F') => {
                                                            // Switch prompt to feedback input
                                                            self.prompt_mode = PromptMode::WizardInput {
                                                                label: "Feedback".into(),
                                                                masked: false,
                                                            };
                                                            self.menu = MenuContent::WizardTrail(vec![
                                                                ("Action".into(), "Rejected with feedback".into()),
                                                            ]);
                                                            // Store approval ID for when feedback is submitted
                                                            self.pending_approval_id = Some(approval_id.clone());
                                                            self.textarea.select_all();
                                                            self.textarea.cut();
                                                            None // Don't send response yet
                                                        }
                                                        KeyCode::Esc => {
                                                            Some(ApprovalDecision::Reject)
                                                        }
                                                        _ => None,
                                                    };
                                                    if let Some(d) = decision {
                                                        self.menu = MenuContent::None;
                                                        let _ = cmd_tx
                                                            .send(EngineCommand::ApprovalResponse {
                                                                id: approval_id,
                                                                decision: d,
                                                            })
                                                            .await;
                                                    }
                                                    continue;
                                                }

                                                // LoopCap hotkeys
                                                if matches!(self.menu, MenuContent::LoopCap) {
                                                    let action = match key.code {
                                                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                                                            Some(koda_core::loop_guard::LoopContinuation::Continue200)
                                                        }
                                                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                                            Some(koda_core::loop_guard::LoopContinuation::Stop)
                                                        }
                                                        _ => None,
                                                    };
                                                    if let Some(a) = action {
                                                        self.menu = MenuContent::None;
                                                        let _ = cmd_tx
                                                            .send(EngineCommand::LoopDecision { action: a })
                                                            .await;
                                                    }
                                                    continue;
                                                }

                                                // Feedback text input during inference
                                                if matches!(self.prompt_mode, PromptMode::WizardInput { .. })
                                                    && self.pending_approval_id.is_some()
                                                {
                                                    match key.code {
                                                        KeyCode::Enter => {
                                                            let feedback = self.textarea.lines().join("\n");
                                                            self.textarea.select_all();
                                                            self.textarea.cut();
                                                            self.prompt_mode = PromptMode::Chat;
                                                            self.menu = MenuContent::None;
                                                            if let Some(aid) = self.pending_approval_id.take() {
                                                                let decision = if feedback.trim().is_empty() {
                                                                    ApprovalDecision::Reject
                                                                } else {
                                                                    ApprovalDecision::RejectWithFeedback { feedback }
                                                                };
                                                                let _ = cmd_tx
                                                                    .send(EngineCommand::ApprovalResponse {
                                                                        id: aid,
                                                                        decision,
                                                                    })
                                                                    .await;
                                                            }
                                                            continue;
                                                        }
                                                        KeyCode::Esc => {
                                                            self.textarea.select_all();
                                                            self.textarea.cut();
                                                            self.prompt_mode = PromptMode::Chat;
                                                            self.menu = MenuContent::None;
                                                            if let Some(aid) = self.pending_approval_id.take() {
                                                                let _ = cmd_tx
                                                                    .send(EngineCommand::ApprovalResponse {
                                                                        id: aid,
                                                                        decision: ApprovalDecision::Reject,
                                                                    })
                                                                    .await;
                                                            }
                                                            continue;
                                                        }
                                                        _ => {
                                                            // Let self.textarea handle the key
                                                            self.textarea.input(Event::Key(key));
                                                            continue;
                                                        }
                                                    }
                                                }

                                                match (key.code, key.modifiers) {
                                                    (KeyCode::Enter, KeyModifiers::NONE) => {
                                                        let text = self.textarea.lines().join("\n");
                                                        if !text.trim().is_empty() {
                                                            self.textarea.select_all();
                                                            self.textarea.cut();
                                                            self.history.push(text.clone());
                                                            tui_history::save_history(&self.history);
                                                            self.history_idx = None;
                                                            self.input_queue.push_back(text);
                                                        }
                                                    }
                                                    (KeyCode::Esc, _) => {
                                                        cancel_token.cancel();
                                                    }
                                                    (KeyCode::Char('c'), m)
                                                        if m.contains(KeyModifiers::CONTROL) =>
                                                    {
                                                        if crate::interrupt::handle_sigint() {
                                                            restore_terminal(&mut self.terminal, self.viewport_height);
                                                            tui_output::err_msg("Force quit.".into());
                                                            std::process::exit(130);
                                                        }
                                                        cancel_token.cancel();
                                                    }
                                                    (KeyCode::BackTab, _) => {
                                                        approval::cycle_mode(&self.shared_mode);
                                                    }
                                                    (KeyCode::Tab, KeyModifiers::NONE) => {
                                                        // Silent Tab completion during inference
                                                        // (no dropdown — would block the event loop)
                                                        let current = self.textarea.lines().join("\n");
                                                        if let Some(completed) = self.completer.complete(&current) {
                                                            self.textarea.select_all();
                                                            self.textarea.cut();
                                                            self.textarea.insert_str(&completed);
                                                        }
                                                    }
                                                    _ => {
                                                        self.completer.reset();
                                                        self.textarea.input(Event::Key(key));
                                                    }
                                                }
                                            }
                                        }
                                        Some(ui_event) = ui_rx.recv() => {
                                            match ui_event {
                                                UiEvent::Engine(EngineEvent::ApprovalRequest {
                                                    id, tool_name, detail, preview,
                                                }) => {
                                                    if preview.is_some() {
                                                        self.renderer.preview_shown = true;
                                                    }
                                                    // Emit diff preview above the viewport
                                                    if let Some(ref prev) = preview {
                                                        let diff_lines = crate::diff_render::render_lines(prev);
                                                        for line in &diff_lines {
                                                            emit_above(&mut self.terminal, line.clone());
                                                        }
                                                    }
                                                    // Show approval hotkey bar in menu_area
                                                    self.menu = MenuContent::Approval {
                                                        id,
                                                        tool_name,
                                                        detail,
                                                    };
                                                    // Hotkey handling is in the self.crossterm_events
                                                    // branch above — no blocking, no self.terminal reinit
                                                }
                                                UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
                                                    // Emit cap info above the viewport
                                                    emit_above(&mut self.terminal, Line::from(vec![
                                                        Span::raw("  "),
                                                        Span::styled(
                                                            format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                                                            Style::default().fg(Color::Yellow),
                                                        ),
                                                    ]));
                                                    for name in &recent_tools {
                                                        emit_above(&mut self.terminal, Line::from(vec![
                                                            Span::raw("    "),
                                                            Span::styled(format!("\u{25cf} {name}"), Style::default().fg(Color::DarkGray)),
                                                        ]));
                                                    }
                                                    // Show hotkey bar in menu_area
                                                    self.menu = MenuContent::LoopCap;
                                                }
                                                UiEvent::Engine(event) => {
                                                    self.renderer.render_to_terminal(event, &mut self.terminal);
                                                }
                                            }
                                        }
                                    }
                                }
                            } // end of pinned turn block

                            // Turn completed — cleanup
                            self.tui_state = TuiState::Idle;
                            self.inference_start = None;
                            crate::interrupt::reset();
                            self.session.cancel = tokio_util::sync::CancellationToken::new();

                            // Commit undo snapshots for this turn
                            if let Ok(mut undo) = self.agent.tools.undo.lock() {
                                undo.commit_turn();
                            }

                            // Drain remaining UI events
                            while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
                                self.renderer.render_to_terminal(e, &mut self.terminal);
                            }

                            // Auto-compact
                            if self.config.auto_compact_threshold > 0 {
                                let ctx_pct = koda_core::context::percentage();
                                if ctx_pct >= self.config.auto_compact_threshold {
                                    let pending = self
                                        .session
                                        .db
                                        .has_pending_tool_calls(&self.session.id)
                                        .await
                                        .unwrap_or(false);
                                    if pending {
                                        if !self.silent_compact_deferred {
                                            emit_above(
                                                &mut self.terminal,
                                                Line::from(vec![
                                                    ratatui::text::Span::raw("  "),
                                                    ratatui::text::Span::styled(
                                                        format!(
                                                            "\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)"
                                                        ),
                                                        Style::default().fg(Color::Yellow),
                                                    ),
                                                ]),
                                            );
                                            self.silent_compact_deferred = true;
                                        }
                                    } else {
                                        self.silent_compact_deferred = false;
                                        emit_above(
                                            &mut self.terminal,
                                            Line::from(vec![
                                                ratatui::text::Span::raw("  "),
                                                ratatui::text::Span::styled(
                                                    format!(
                                                        "\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."
                                                    ),
                                                    Style::default().fg(Color::Cyan),
                                                ),
                                            ]),
                                        );
                                        match koda_core::compact::compact_session(
                                            &self.session.db,
                                            &self.session.id,
                                            self.config.max_context_tokens,
                                            &self.config.model_settings,
                                            &self.provider,
                                        )
                                        .await
                                        {
                                            Ok(Ok(result)) => {
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::styled(
                                                        format!(
                                                            "  \u{2713} Compacted {} messages \u{2192} ~{} tokens",
                                                            result.deleted, result.summary_tokens
                                                        ),
                                                        Style::default().fg(Color::Green),
                                                    ),
                                                );
                                            }
                                            Ok(Err(_skip)) => {} // silently skip
                                            Err(e) => {
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::styled(
                                                        format!(
                                                            "  \u{2717} Auto-compact failed: {e:#}"
                                                        ),
                                                        Style::default().fg(Color::Red),
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            // Loop back to drain queue before blocking on keyboard
                            continue;
                        }
                    }
                }
            }

            // Redraw viewport (resize if self.textarea grew/shrank)
            let mode = approval::read_mode(&self.shared_mode);
            let ctx = koda_core::context::percentage() as u32;
            maybe_resize_viewport(
                &mut self.terminal,
                &self.textarea,
                &mut self.viewport_height,
            )?;
            self.terminal.draw(|f| {
                draw_viewport(
                    f,
                    &self.textarea,
                    &self.config.model,
                    "koda",
                    mode,
                    ctx,
                    self.tui_state,
                    &self.prompt_mode,
                    self.input_queue.len(),
                    self.inference_start
                        .map(|s| s.elapsed().as_secs())
                        .unwrap_or(0),
                    self.renderer.last_turn_stats.as_ref(),
                    &self.menu,
                );
            })?;

            // ── Idle: wait for keyboard input ────────────────────

            tokio::select! {
                Some(Ok(ev)) = self.crossterm_events.next() => {
                    if let Event::Resize(_, _) = ev {
                        // Terminal resized while idle — erase stale viewport and reinit.
                        reinit_viewport_in_place(&mut self.terminal, self.viewport_height, self.viewport_height)?;
                    } else if let Event::Key(key) = ev {
                        // ── Slash menu key interception ───────────
                        // When a self.menu is active, intercept navigation
                        // and selection keys before normal handling.
                        if !self.menu.is_none() {
                            let is_up = key.code == KeyCode::Up
                                || (key.code == KeyCode::Char('k')
                                    && key.modifiers.contains(KeyModifiers::CONTROL));
                            let is_down = key.code == KeyCode::Down
                                || key.code == KeyCode::Tab
                                || (key.code == KeyCode::Char('j')
                                    && key.modifiers.contains(KeyModifiers::CONTROL));

                            if is_up {
                                match &mut self.menu {
                                    MenuContent::Slash(dd) => dd.up(),
                                    MenuContent::Model(dd) => dd.up(),
                                    MenuContent::Provider(dd) => dd.up(),
                                    MenuContent::Session(dd) => dd.up(),
                                    MenuContent::File { dropdown: dd, .. } => dd.up(),
                                    MenuContent::Approval { .. } | MenuContent::LoopCap | MenuContent::WizardTrail(_) | MenuContent::None => {}
                                }
                                continue;
                            } else if is_down {
                                match &mut self.menu {
                                    MenuContent::Slash(dd) => dd.down(),
                                    MenuContent::Model(dd) => dd.down(),
                                    MenuContent::Provider(dd) => dd.down(),
                                    MenuContent::Session(dd) => dd.down(),
                                    MenuContent::File { dropdown: dd, .. } => dd.down(),
                                    MenuContent::Approval { .. } | MenuContent::LoopCap | MenuContent::WizardTrail(_) | MenuContent::None => {}
                                }
                                continue;
                            }

                            match key.code {
                                KeyCode::Enter => {
                                    match &self.menu {
                                        MenuContent::Slash(dd) => {
                                            if let Some(item) = dd.selected_item() {
                                                let cmd = item.command.to_string();
                                                self.textarea.select_all();
                                                self.textarea.cut();
                                                self.textarea.insert_str(&cmd);
                                            }
                                        }
                                        MenuContent::Model(dd) => {
                                            if let Some(item) = dd.selected_item() {
                                                let model_id = item.id.clone();
                                                self.config.model = model_id.clone();
                                                self.config.model_settings.model = model_id.clone();
                                                self.config.recalculate_model_derived();
                                                {
                                                    let prov = self.provider.read().await;
                                                    self.config.query_and_apply_capabilities(prov.as_ref()).await;
                                                }
                                                crate::tui_wizards::save_provider(&self.config);
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::styled(
                                                        format!("  \u{2714} Model set to: {model_id}"),
                                                        Style::default().fg(Color::Green),
                                                    ),
                                                );
                                                self.renderer.model = model_id;
                                            }
                                        }
                                        MenuContent::Provider(dd) => {
                                            if let Some(item) = dd.selected_item() {
                                                let key = item.key;
                                                let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
                                                let base_url = ptype.default_base_url().to_string();
                                                let provider_name = item.name.to_string();

                                                if ptype.requires_api_key() {
                                                    // Start wizard: need API key
                                                    let env_name = ptype.env_key_name().to_string();
                                                    self.menu = MenuContent::WizardTrail(vec![
                                                        ("Provider".into(), provider_name),
                                                    ]);
                                                    self.prompt_mode = PromptMode::WizardInput {
                                                        label: format!("API key for {}", ptype),
                                                        masked: true,
                                                    };
                                                    self.provider_wizard = Some(ProviderWizard::NeedApiKey {
                                                        provider_type: ptype,
                                                        base_url,
                                                        env_name,
                                                    });
                                                    self.textarea.select_all();
                                                    self.textarea.cut();
                                                } else {
                                                    // Local self.provider: need URL
                                                    self.menu = MenuContent::WizardTrail(vec![
                                                        ("Provider".into(), provider_name),
                                                    ]);
                                                    self.prompt_mode = PromptMode::WizardInput {
                                                        label: format!("{} URL", ptype),
                                                        masked: false,
                                                    };
                                                    self.provider_wizard = Some(ProviderWizard::NeedUrl {
                                                        provider_type: ptype,
                                                    });
                                                    self.textarea.select_all();
                                                    self.textarea.cut();
                                                    // Pre-fill with default URL
                                                    self.textarea.insert_str(&base_url);
                                                }
                                            }
                                            continue;
                                        }
                                        MenuContent::Session(dd) => {
                                            if let Some(item) = dd.selected_item() {
                                                if item.is_current {
                                                    emit_above(
                                                        &mut self.terminal,
                                                        Line::styled(
                                                            "  Already in this session.",
                                                            Style::default().fg(Color::DarkGray),
                                                        ),
                                                    );
                                                } else {
                                                    let target_id = item.id.clone();
                                                    let short = &item.short_id;
                                                    self.session.id = target_id;
                                                    emit_above(
                                                        &mut self.terminal,
                                                        Line::from(vec![
                                                            Span::styled(
                                                                "  \u{2714} ",
                                                                Style::default().fg(Color::Green),
                                                            ),
                                                            Span::raw("Resumed session "),
                                                            Span::styled(
                                                                short.to_string(),
                                                                Style::default().fg(Color::Cyan),
                                                            ),
                                                        ]),
                                                    );
                                                }
                                            }
                                        }
                                        MenuContent::File {
                                            dropdown,
                                            prefix,
                                        } => {
                                            if let Some(item) = dropdown.selected_item() {
                                                let replacement =
                                                    format!("{prefix}@{}", item.path);
                                                self.textarea.select_all();
                                                self.textarea.cut();
                                                self.textarea.insert_str(&replacement);
                                            }
                                        }
                                        MenuContent::Approval { .. }
                                        | MenuContent::LoopCap
                                        | MenuContent::WizardTrail(_)
                                        | MenuContent::None => {}
                                    }
                                    self.menu = MenuContent::None;
                                    continue;
                                }
                                KeyCode::Esc => {
                                    self.menu = MenuContent::None;
                                    // Cancel wizard if active
                                    if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
                                        self.prompt_mode = PromptMode::Chat;
                                        self.provider_wizard = None;
                                        self.textarea.select_all();
                                        self.textarea.cut();
                                    }
                                    continue;
                                }
                                _ => {
                                    // Fall through — let normal handlers process
                                    // (typing filters the slash self.menu via the _ arm)
                                }
                            }
                        }
                        match (key.code, key.modifiers) {
                            // Shift+Enter or Alt+Enter → insert newline
                            // Note: Shift+Enter only works on terminals with kitty
                            // keyboard protocol. Alt+Enter works everywhere.
                            (KeyCode::Enter, m)
                                if m.contains(KeyModifiers::SHIFT)
                                    || m.contains(KeyModifiers::ALT) =>
                            {
                                self.textarea.insert_newline();
                            }
                            (KeyCode::Enter, KeyModifiers::NONE) => {
                                // Wizard input mode: submit value to wizard
                                if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
                                    let value = self.textarea.lines().join("");
                                    self.textarea.select_all();
                                    self.textarea.cut();

                                    if let Some(wizard) = self.provider_wizard.take() {
                                        match wizard {
                                            ProviderWizard::NeedApiKey {
                                                provider_type,
                                                base_url,
                                                env_name,
                                            } => {
                                                if !value.is_empty() {
                                                    koda_core::runtime_env::set(&env_name, &value);
                                                    // Persist to keystore
                                                    if let Ok(mut store) =
                                                        koda_core::keystore::KeyStore::load()
                                                    {
                                                        store.set(&env_name, &value);
                                                        let _ = store.save();
                                                    }
                                                    let masked =
                                                        koda_core::keystore::mask_key(&value);
                                                    emit_above(
                                                        &mut self.terminal,
                                                        Line::styled(
                                                            format!(
                                                                "  \u{2714} {env_name} set to {masked}"
                                                            ),
                                                            Style::default().fg(Color::Green),
                                                        ),
                                                    );
                                                }
                                                // Apply self.provider self.config
                                                self.config.provider_type = provider_type.clone();
                                                self.config.base_url = base_url;
                                                self.config.model =
                                                    provider_type.default_model().to_string();
                                                self.config.model_settings.model = self.config.model.clone();
                                                self.config.recalculate_model_derived();
                                                *self.provider.write().await =
                                                    koda_core::providers::create_provider(&self.config);
                                                crate::tui_wizards::save_provider(&self.config);
                                                // Verify connection + auto-select model
                                                let prov = self.provider.read().await;
                                                if let Ok(models) = prov.list_models().await {
                                                    if let Some(first) = models.first() {
                                                        self.config.model = first.id.clone();
                                                        self.config.model_settings.model =
                                                            self.config.model.clone();
                                                        self.config.recalculate_model_derived();
                                                    }
                                                    self.config
                                                        .query_and_apply_capabilities(prov.as_ref())
                                                        .await;
                                                    self.completer.set_model_names(
                                                        models
                                                            .iter()
                                                            .map(|m| m.id.clone())
                                                            .collect(),
                                                    );
                                                }
                                                self.renderer.model = self.config.model.clone();
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::styled(
                                                        format!(
                                                            "  \u{2714} Provider: {} ({})",
                                                            self.config.provider_type, self.config.model
                                                        ),
                                                        Style::default().fg(Color::Green),
                                                    ),
                                                );
                                            }
                                            ProviderWizard::NeedUrl { provider_type } => {
                                                let url = if value.is_empty() {
                                                    provider_type
                                                        .default_base_url()
                                                        .to_string()
                                                } else {
                                                    value
                                                };
                                                self.config.provider_type = provider_type;
                                                self.config.base_url = url.clone();
                                                self.config.model = self.config
                                                    .provider_type
                                                    .default_model()
                                                    .to_string();
                                                self.config.model_settings.model = self.config.model.clone();
                                                self.config.recalculate_model_derived();
                                                *self.provider.write().await =
                                                    koda_core::providers::create_provider(&self.config);
                                                crate::tui_wizards::save_provider(&self.config);
                                                let prov = self.provider.read().await;
                                                if let Ok(models) = prov.list_models().await {
                                                    if let Some(first) = models.first() {
                                                        self.config.model = first.id.clone();
                                                        self.config.model_settings.model =
                                                            self.config.model.clone();
                                                        self.config.recalculate_model_derived();
                                                    }
                                                    self.config
                                                        .query_and_apply_capabilities(prov.as_ref())
                                                        .await;
                                                    self.completer.set_model_names(
                                                        models
                                                            .iter()
                                                            .map(|m| m.id.clone())
                                                            .collect(),
                                                    );
                                                }
                                                self.renderer.model = self.config.model.clone();
                                                emit_above(
                                                    &mut self.terminal,
                                                    Line::styled(
                                                        format!(
                                                            "  \u{2714} Provider: {} at {}",
                                                            self.config.provider_type, url
                                                        ),
                                                        Style::default().fg(Color::Green),
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    // Reset wizard state
                                    self.prompt_mode = PromptMode::Chat;
                                    self.menu = MenuContent::None;
                                    continue;
                                }

                                // Paste detection: peek ahead for more input.
                                // If characters arrive within 30ms, it's a paste —
                                // insert newline instead of submitting.
                                let is_paste = tokio::time::timeout(
                                    std::time::Duration::from_millis(30),
                                    self.crossterm_events.next(),
                                )
                                .await;

                                match is_paste {
                                    Ok(Some(Ok(Event::Key(next_key)))) => {
                                        // More input arrived quickly — it's a paste
                                        self.textarea.insert_newline();
                                        self.textarea.input(Event::Key(next_key));
                                    }
                                    _ => {
                                        // Timeout or no event — real Enter, submit
                                        let text = self.textarea.lines().join("\n");
                                        if !text.trim().is_empty() {
                                            self.textarea.select_all();
                                            self.textarea.cut();
                                            self.history.push(text.clone());
                                            tui_history::save_history(&self.history);
                                            self.history_idx = None;
                                            let mode = approval::read_mode(&self.shared_mode);
                                            let icon = match mode {
                                                ApprovalMode::Safe => "🔍",
                                                ApprovalMode::Strict => "🔒",
                                                ApprovalMode::Auto => "⚡",
                                            };
                                            emit_above(&mut self.terminal, Line::from(vec![
                                                Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                                                Span::raw(text.clone()),
                                            ]));
                                            self.pending_command = Some(text);
                                        }
                                    }
                                }
                            }
                            (KeyCode::Up, KeyModifiers::NONE)
                            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                                if !self.history.is_empty() {
                                    let idx = match self.history_idx {
                                        None => self.history.len() - 1,
                                        Some(i) => i.saturating_sub(1),
                                    };
                                    self.history_idx = Some(idx);
                                    self.textarea.select_all();
                                    self.textarea.cut();
                                    self.textarea.insert_str(&self.history[idx]);
                                }
                            }
                            (KeyCode::Down, KeyModifiers::NONE)
                            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                                if let Some(idx) = self.history_idx {
                                    if idx + 1 < self.history.len() {
                                        self.history_idx = Some(idx + 1);
                                        self.textarea.select_all();
                                        self.textarea.cut();
                                        self.textarea.insert_str(&self.history[idx + 1]);
                                    } else {
                                        self.history_idx = None;
                                        self.textarea.select_all();
                                        self.textarea.cut();
                                    }
                                }
                            }
                            (KeyCode::Esc, _) => {
                                self.textarea.select_all();
                                self.textarea.cut();
                                self.history_idx = None;
                            }
                            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                                self.textarea.select_all();
                                self.textarea.cut();
                                self.history_idx = None;
                            }
                            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                if self.textarea.lines().join("").trim().is_empty() {
                                    self.should_quit = true;
                                }
                            }
                            (KeyCode::BackTab, _) => {
                                approval::cycle_mode(&self.shared_mode);
                                // Status bar updates on next draw — no scrollback noise
                            }
                            (KeyCode::Tab, KeyModifiers::NONE) => {
                                // Tab cycles through completions (single insertion).
                                // Multi-match dropdowns are now handled by the
                                // auto-dropdowns on / and @ in the _ handler.
                                let current = self.textarea.lines().join("\n");
                                if let Some(completed) = self.completer.complete(&current) {
                                    self.textarea.select_all();
                                    self.textarea.cut();
                                    self.textarea.insert_str(&completed);
                                    self.completer.reset();
                                }
                            }
                            _ => {
                                self.history_idx = None;
                                self.completer.reset();
                                self.textarea.input(Event::Key(key));

                                // Update self.menu state reactively based on input
                                let after_input = self.textarea.lines().join("\n");
                                let trimmed_after = after_input.trim_end();

                                if trimmed_after.starts_with('/') && !trimmed_after.contains(' ') {
                                    // Slash command dropdown
                                    if let Some(dd) = crate::widgets::slash_menu::from_input(
                                        crate::completer::SLASH_COMMANDS,
                                        trimmed_after,
                                    ) {
                                        self.menu = MenuContent::Slash(dd);
                                    } else if matches!(self.menu, MenuContent::Slash(_)) {
                                        self.menu = MenuContent::None;
                                    }
                                } else if let Some(at_pos) =
                                    crate::completer::find_last_at_token(trimmed_after)
                                {
                                    // @file dropdown
                                    let partial = &trimmed_after[at_pos + 1..];
                                    let prefix = &trimmed_after[..at_pos];
                                    let matches =
                                        crate::completer::list_path_matches_public(
                                            &self.project_root,
                                            partial,
                                        );
                                    if !matches.is_empty() {
                                        let items: Vec<crate::widgets::file_menu::FileItem> = matches
                                            .iter()
                                            .map(|p| crate::widgets::file_menu::FileItem {
                                                path: p.clone(),
                                                is_dir: p.ends_with('/'),
                                            })
                                            .collect();
                                        let dd = crate::widgets::dropdown::DropdownState::new(
                                            items,
                                            "\u{1f4c2} Files",
                                        );
                                        self.menu = MenuContent::File {
                                            dropdown: dd,
                                            prefix: prefix.to_string(),
                                        };
                                    } else if matches!(self.menu, MenuContent::File { .. }) {
                                        self.menu = MenuContent::None;
                                    }
                                } else {
                                    // Clear menu if it was a slash or file menu
                                    if matches!(self.menu, MenuContent::Slash(_) | MenuContent::File { .. }) {
                                        self.menu = MenuContent::None;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
