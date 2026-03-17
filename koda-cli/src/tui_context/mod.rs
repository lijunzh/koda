//! TUI shared context — the mutable state struct for the event loop.
//!
//! ## Module layout
//!
//! - **mod.rs** — struct definition, constructor, lifecycle, command dispatch
//! - **events.rs** — keyboard, mouse, paste, clipboard, history input handling
//! - **menus.rs** — dropdown pickers, menu navigation, wizard state machines

mod events;
mod menus;

use events::load_history;
pub(crate) use events::save_history;

use crate::input;
use crate::scroll_buffer::ScrollBuffer;
use crate::sink::UiEvent;
use crate::tui_commands::{self, SlashAction};
use crate::tui_render::TuiRenderer;
use crate::tui_types::{MenuContent, PromptMode, ProviderWizard, Term, TuiState};
use crate::tui_viewport::{draw_viewport, init_terminal, restore_terminal};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::Role;
use koda_core::engine::EngineCommand;
use koda_core::persistence::Persistence;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use ratatui_textarea::TextArea;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

/// All mutable TUI state, extracted from `run()`'s local variables.
///
/// Methods are split across submodules by concern:
/// - **mod.rs** — lifecycle (new, draw, cleanup, event loop, dispatch)
/// - **events.rs** — keyboard, mouse, paste, clipboard, history
/// - **menus.rs** — dropdown pickers, menu navigation, wizards
pub(crate) struct TuiContext {
    // ── UI state ─────────────────────────────────────────────
    pub terminal: Term,
    pub textarea: TextArea<'static>,
    pub renderer: TuiRenderer,
    pub scroll_buffer: ScrollBuffer,
    pub crossterm_events: EventStream,

    // ── Interaction state ─────────────────────────────────────
    pub tui_state: TuiState,
    pub menu: MenuContent,
    pub prompt_mode: PromptMode,
    pub provider_wizard: Option<ProviderWizard>,
    pub pending_approval_id: Option<String>,

    // ── Control flow ──────────────────────────────────────────
    pub paste_blocks: Vec<input::PasteBlock>,
    pub input_queue: VecDeque<String>,
    pub pending_command: Option<String>,
    pub should_quit: bool,
    pub silent_compact_deferred: bool,
    pub inference_start: Option<std::time::Instant>,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub completer: crate::completer::InputCompleter,
    pub mouse_selection: Option<crate::mouse_select::Selection>,
    /// Y-coordinate of the history area top edge (for mouse mapping).
    pub history_area_y: u16,
    /// Height of the history area in rows.
    pub history_area_height: u16,

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

/// Outcome of dispatching a single command.
pub(crate) enum CommandOutcome {
    /// No input was available to dispatch.
    NoInput,
    /// Command was fully handled (slash command, dropdown opened, etc.).
    Handled,
    /// Non-slash input — ready to start an inference turn.
    StartInference {
        pending_images: Option<Vec<koda_core::providers::ImageData>>,
    },
    /// /quit was invoked.
    Quit,
}

impl TuiContext {
    /// Initialize all TUI state. Call before entering the event loop.
    ///
    /// This handles provider setup, auto-detection, terminal init,
    /// onboarding, and everything that `run()` used to do before `loop {`.
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

        // Collect startup lines (will be pushed into scroll buffer after init)
        let recent = db.recent_user_messages(3).await.unwrap_or_default();
        let mut startup_lines = crate::startup::collect_startup_lines(&config, &recent);

        if let Ok(Some(latest)) = version_check.await
            && let Some((current, latest)) = koda_core::version::update_available(&latest)
        {
            startup_lines.extend(crate::startup::update_notice_lines(current, &latest));
        }

        let agent =
            Arc::new(koda_core::agent::KodaAgent::new(&config, project_root.clone()).await?);

        let session = KodaSession::new(
            session_id,
            agent.clone(),
            db.clone(),
            &config,
            ApprovalMode::Auto,
        )
        .await;

        crate::startup::purge_nudge(&db, &mut startup_lines).await;

        let shared_mode = approval::new_shared_mode(ApprovalMode::Auto);

        // Terminal + textarea
        let terminal = init_terminal()?;

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

        // Load and render historical conversation on session resume
        let (history_lines, oldest_msg_id) = {
            use koda_core::persistence::Persistence;
            match session.db.load_context(&session.id).await {
                Ok(msgs) if !msgs.is_empty() => {
                    let oldest_id = msgs.first().map(|m| m.id);
                    let lines = crate::history_render::render_history_messages(&msgs);
                    (lines, oldest_id)
                }
                _ => (Vec::new(), None),
            }
        };

        Ok(Self {
            terminal,
            textarea,
            renderer,
            scroll_buffer: {
                let mut buf = ScrollBuffer::new(2500);
                buf.push_lines(startup_lines);
                if !history_lines.is_empty() {
                    buf.push_lines(history_lines);
                }
                if let Some(id) = oldest_msg_id {
                    buf.set_oldest_message_id(id);
                }
                buf
            },
            crossterm_events: EventStream::new(),
            tui_state: TuiState::Idle,
            menu,
            prompt_mode: PromptMode::Chat,
            provider_wizard: None,
            pending_approval_id: None,
            paste_blocks: Vec::new(),
            input_queue: VecDeque::new(),
            pending_command: None,
            should_quit: false,
            silent_compact_deferred: false,
            inference_start: None,
            history: load_history(),
            history_idx: None,
            completer,
            mouse_selection: None,
            history_area_y: 0,
            history_area_height: 0,
            config,
            provider,
            session,
            shared_mode,
            agent,
            project_root,
        })
    }

    /// Draw the fullscreen viewport.
    pub fn draw(&mut self) -> Result<()> {
        // Clamp scroll offset to valid bounds before rendering.
        // Necessary after terminal resize changes wrapping math.
        let (w, h) = self.term_dims();
        self.scroll_buffer.clamp_offset(w, h);

        let mode = approval::read_mode(&self.shared_mode);
        let ctx = koda_core::context::percentage() as u32;
        let tui_state = self.tui_state;
        let prompt_mode = &self.prompt_mode;
        let queue_len = self.input_queue.len();
        let elapsed = self
            .inference_start
            .map(|s| s.elapsed().as_secs())
            .unwrap_or(0);
        let last_turn = self.renderer.last_turn_stats.as_ref();
        let menu = &self.menu;
        let scroll_buffer = &self.scroll_buffer;
        let textarea = &self.textarea;
        let config = &self.config;
        let selection = self.mouse_selection.as_ref();

        let mut history_rect = None;
        if let Err(e) = self.terminal.draw(|f| {
            history_rect = Some(draw_viewport(
                f,
                textarea,
                &config.model,
                mode,
                ctx,
                tui_state,
                prompt_mode,
                queue_len,
                elapsed,
                last_turn,
                menu,
                scroll_buffer,
                selection,
            ));
        }) {
            tracing::debug!("draw skipped: {e}");
        }
        if let Some(rect) = history_rect {
            self.history_area_y = rect.y;
            self.history_area_height = rect.height;
        }
        Ok(())
    }

    /// Push a message line into the scroll buffer.
    pub fn emit(&mut self, line: Line<'static>) {
        self.scroll_buffer.push(line);
    }

    /// Clean up terminal and print exit info.
    pub async fn cleanup(&mut self) {
        restore_terminal(&mut self.terminal);
        crate::startup::print_resume_hint(&self.session.id);
    }

    /// The main event loop.
    pub async fn run_event_loop(
        &mut self,
        ui_tx: &mpsc::UnboundedSender<UiEvent>,
        ui_rx: &mut mpsc::UnboundedReceiver<UiEvent>,
        cmd_tx: &mpsc::Sender<EngineCommand>,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    ) -> Result<()> {
        loop {
            if self.should_quit {
                break;
            }

            // ── Dispatch queued / pending commands ───────────
            if self.tui_state == TuiState::Idle
                && let Some(raw) = self.dequeue_input()
            {
                match self.dispatch_command(&raw).await {
                    CommandOutcome::NoInput => {}
                    CommandOutcome::Handled => continue,
                    CommandOutcome::Quit => {
                        self.should_quit = true;
                        continue;
                    }
                    CommandOutcome::StartInference { pending_images } => {
                        self.run_inference_turn(pending_images, ui_tx, ui_rx, cmd_tx, cmd_rx)
                            .await?;
                        continue;
                    }
                }
            }

            // Redraw viewport (resize if textarea grew/shrank)
            self.draw()?;

            // ── Idle: wait for keyboard input ────────────────
            tokio::select! {
                Some(Ok(ev)) = self.crossterm_events.next() => {
                    self.handle_idle_event(ev).await?;
                }
            }
        }
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════
    // Command dispatch (slash commands, dropdown openers, inference prep)
    // ═══════════════════════════════════════════════════════════

    /// Dequeue the next pending or queued input string, if any.
    fn dequeue_input(&mut self) -> Option<String> {
        if let Some(cmd) = self.pending_command.take() {
            return Some(cmd);
        }
        if let Some(queued) = self.input_queue.pop_front() {
            let mode = approval::read_mode(&self.shared_mode);
            let icon = match mode {
                ApprovalMode::Confirm => "🔒",
                ApprovalMode::Auto => "⚡",
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                Span::raw(queued.clone()),
            ]));
            return Some(queued);
        }
        None
    }

    /// Dispatch a raw input string.
    async fn dispatch_command(&mut self, raw: &str) -> CommandOutcome {
        let input = raw.trim().to_string();
        if input.is_empty() {
            return CommandOutcome::NoInput;
        }
        if input.starts_with('/') {
            return self.dispatch_slash(&input).await;
        }
        self.prepare_inference_start(&input).await
    }

    async fn dispatch_slash(&mut self, input: &str) -> CommandOutcome {
        if input.trim() == "/model" {
            self.open_model_picker().await;
            return CommandOutcome::Handled;
        }
        if input.trim() == "/provider" {
            self.open_provider_picker();
            return CommandOutcome::Handled;
        }
        if let Some(name) = input.trim().strip_prefix("/provider ") {
            self.start_provider_wizard(name.trim());
            return CommandOutcome::Handled;
        }
        if input.trim() == "/sessions" {
            self.open_session_picker().await;
            return CommandOutcome::Handled;
        }

        let action = tui_commands::handle_slash_command(
            &mut self.scroll_buffer,
            input,
            &mut self.config,
            &self.provider,
            &mut self.session,
            &self.shared_mode,
            &mut self.renderer,
            &self.project_root,
            &self.agent,
            &mut self.pending_command,
            &mut self.menu,
        )
        .await;

        match action {
            SlashAction::Continue => {
                self.reinit_after_slash().await;
                CommandOutcome::Handled
            }
            SlashAction::Quit => {
                self.scroll_buffer.push(Line::styled(
                    "\u{1f43b} Goodbye!",
                    Style::default().fg(Color::Cyan),
                ));
                CommandOutcome::Quit
            }
        }
    }

    async fn reinit_after_slash(&mut self) {
        // Fullscreen mode: no terminal reinit needed.
        // Just refresh completer state and model name.
        {
            let prov = self.provider.read().await;
            if let Ok(models) = prov.list_models().await {
                self.completer
                    .set_model_names(models.iter().map(|m| m.id.clone()).collect());
            }
        }
        self.renderer.model = self.config.model.clone();
    }

    async fn prepare_inference_start(&mut self, input: &str) -> CommandOutcome {
        let mut processed = input::process_input(input, &self.project_root);
        processed.paste_blocks = std::mem::take(&mut self.paste_blocks);

        for (i, _img) in processed.images.iter().enumerate() {
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("\u{1f5bc} Image {}", i + 1),
                    Style::default().fg(Color::Magenta),
                ),
            ]));
        }

        let mut user_message =
            if let Some(context) = input::format_context_files(&processed.context_files) {
                for f in &processed.context_files {
                    self.scroll_buffer.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("\u{1f4ce} {}", f.path),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]));
                }
                format!("{}\n\n{context}", processed.prompt)
            } else {
                processed.prompt.clone()
            };

        if let Some(pasted) = input::format_paste_blocks(&processed.paste_blocks) {
            user_message = format!("{user_message}\n\n{pasted}");
        }

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

        CommandOutcome::StartInference { pending_images }
    }

    // ═══════════════════════════════════════════════════════════
    // Idle-mode event handling (keyboard, paste, menu, wizard, history)
    // ═══════════════════════════════════════════════════════════
}
