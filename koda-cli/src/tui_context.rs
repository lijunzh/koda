//! TUI shared context — the mutable state struct for the event loop.
//!
//! Holds all mutable locals that were previously captured in `run()`'s
//! closure scope. Methods on this struct replace inline blocks.
//! See #209.

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
        ui_tx: &mpsc::Sender<UiEvent>,
        ui_rx: &mut mpsc::Receiver<UiEvent>,
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

    // ── Dropdown openers ─────────────────────────────────────────

    async fn open_model_picker(&mut self) {
        let prov = self.provider.read().await;
        match prov.list_models().await {
            Ok(models) if !models.is_empty() => {
                let items: Vec<crate::widgets::model_menu::ModelItem> = models
                    .iter()
                    .map(|m| crate::widgets::model_menu::ModelItem {
                        id: m.id.clone(),
                        is_current: m.id == self.config.model,
                    })
                    .collect();
                let mut dd =
                    crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a model");
                if let Some(idx) = dd.filtered.iter().position(|m| m.is_current) {
                    dd.selected = idx;
                    let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                    if idx >= max_vis {
                        dd.scroll_offset = idx + 1 - max_vis;
                    }
                }
                self.menu = MenuContent::Model(dd);
            }
            Ok(_) => {
                self.scroll_buffer.push(Line::styled(
                    "  \u{26a0} No models available",
                    Style::default().fg(Color::Yellow),
                ));
            }
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Failed to list models: {e}"),
                    Style::default().fg(Color::Red),
                ));
            }
        }
    }

    fn open_provider_picker(&mut self) {
        let providers = crate::repl::PROVIDERS;
        let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
            .iter()
            .map(
                |(key, name, desc)| crate::widgets::provider_menu::ProviderItem {
                    key,
                    name,
                    description: desc,
                    is_current: koda_core::config::ProviderType::from_url_or_name("", Some(key))
                        == self.config.provider_type,
                },
            )
            .collect();
        let mut dd =
            crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Select a provider");
        if let Some(idx) = dd.filtered.iter().position(|p| p.is_current) {
            dd.selected = idx;
            let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
            if idx >= max_vis {
                dd.scroll_offset = idx + 1 - max_vis;
            }
        }
        self.menu = MenuContent::Provider(dd);
    }

    fn start_provider_wizard(&mut self, name: &str) {
        let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(name));
        let base_url = ptype.default_base_url().to_string();
        let provider_name = ptype.to_string();

        if ptype.requires_api_key() {
            let env_name = ptype.env_key_name().to_string();
            let has_key = koda_core::runtime_env::is_set(&env_name);
            let label = if has_key {
                format!("API key for {} (Enter to keep current)", ptype)
            } else {
                format!("API key for {}", ptype)
            };
            self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
            self.prompt_mode = PromptMode::WizardInput { label };
            self.provider_wizard = Some(ProviderWizard::NeedApiKey {
                provider_type: ptype,
                base_url,
                env_name,
            });
            self.textarea.select_all();
            self.textarea.cut();
        } else {
            self.menu = MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
            self.prompt_mode = PromptMode::WizardInput {
                label: format!("{} URL", ptype),
            };
            self.provider_wizard = Some(ProviderWizard::NeedUrl {
                provider_type: ptype,
            });
            self.textarea.select_all();
            self.textarea.cut();
            self.textarea.insert_str(&base_url);
        }
    }

    async fn open_session_picker(&mut self) {
        match self.session.db.list_sessions(10, &self.project_root).await {
            Ok(sessions) if !sessions.is_empty() => {
                let items: Vec<crate::widgets::session_menu::SessionItem> = sessions
                    .iter()
                    .map(|s| crate::widgets::session_menu::SessionItem {
                        id: s.id.clone(),
                        short_id: s.id[..8.min(s.id.len())].to_string(),
                        created_at: s.created_at.clone(),
                        message_count: s.message_count,
                        total_tokens: s.total_tokens,
                        is_current: s.id == self.session.id,
                    })
                    .collect();
                let mut dd =
                    crate::widgets::dropdown::DropdownState::new(items, "\u{1f43b} Sessions");
                if let Some(idx) = dd.filtered.iter().position(|s| s.is_current) {
                    dd.selected = idx;
                    let max_vis = crate::widgets::dropdown::MAX_VISIBLE;
                    if idx >= max_vis {
                        dd.scroll_offset = idx + 1 - max_vis;
                    }
                }
                self.menu = MenuContent::Session(dd);
            }
            Ok(_) => {
                self.scroll_buffer.push(Line::styled(
                    "  No other sessions found.",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Err(e) => {
                self.scroll_buffer.push(Line::styled(
                    format!("  \u{2717} Error: {e}"),
                    Style::default().fg(Color::Red),
                ));
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

    async fn handle_idle_event(&mut self, ev: Event) -> anyhow::Result<bool> {
        match ev {
            Event::Resize(_, _) => {
                // Clamp scroll offset for the new terminal dimensions.
                let (w, h) = self.term_dims();
                self.scroll_buffer.clamp_offset(w, h);
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
            }
            Event::Paste(text) => {
                self.handle_idle_paste(&text);
            }
            Event::Key(key) => {
                return self.handle_idle_key(key).await;
            }
            _ => {}
        }
        Ok(true)
    }

    fn handle_idle_paste(&mut self, text: &str) {
        let char_count = text.chars().count();
        if matches!(self.prompt_mode, PromptMode::WizardInput { .. })
            || char_count < input::PASTE_BLOCK_THRESHOLD
        {
            self.textarea.insert_str(text);
        } else {
            self.paste_blocks.push(input::PasteBlock {
                content: text.to_string(),
                char_count,
            });
            let label = format!("\u{1f4cb} Pasted text ({char_count} chars)");
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label, Style::default().fg(Color::Yellow)),
            ]));
            let preview: String = text.chars().take(80).collect();
            let preview = preview.replace('\n', "\u{21b5}");
            let preview = if char_count > 80 {
                format!("{preview}\u{2026}")
            } else {
                preview
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(preview, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    async fn handle_idle_key(&mut self, key: crossterm::event::KeyEvent) -> anyhow::Result<bool> {
        if !self.menu.is_none()
            && let Some(consumed) = self.handle_menu_key(key).await
        {
            return Ok(consumed);
        }

        match (key.code, key.modifiers) {
            (KeyCode::Enter, m)
                if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
            {
                self.textarea.insert_newline();
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                return self.handle_idle_enter().await;
            }
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_up();
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.history_down();
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
            (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Ctrl+L: jump to bottom (re-engage sticky)
                self.scroll_buffer.scroll_to_bottom();
            }
            // Scroll keys
            (KeyCode::PageUp, _) => {
                let (w, h) = self.term_dims();
                self.scroll_buffer.scroll_up(20, w, h);
            }
            (KeyCode::PageDown, _) => {
                self.scroll_buffer.scroll_down(20);
            }
            (KeyCode::Home, _) => {
                let (w, h) = self.term_dims();
                self.scroll_buffer.scroll_to_top(w, h);
            }
            (KeyCode::End, _) => {
                self.scroll_buffer.scroll_to_bottom();
            }
            // Clipboard: Ctrl+Y = copy last code block
            (KeyCode::Char('y'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.copy_to_clipboard(m.contains(KeyModifiers::SHIFT));
            }
            (KeyCode::BackTab, _) => {
                approval::cycle_mode(&self.shared_mode);
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
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
                self.update_reactive_menu();
            }
        }
        Ok(true)
    }

    async fn handle_idle_enter(&mut self) -> anyhow::Result<bool> {
        if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
            self.handle_wizard_submit().await;
            return Ok(true);
        }

        let text = self.textarea.lines().join("\n");
        if !text.trim().is_empty() {
            self.textarea.select_all();
            self.textarea.cut();
            self.history.push(text.clone());
            save_history(&self.history);
            self.history_idx = None;
            let mode = approval::read_mode(&self.shared_mode);
            let icon = match mode {
                ApprovalMode::Confirm => "\u{1f512}",
                ApprovalMode::Auto => "\u{26a1}",
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                Span::raw(text.clone()),
            ]));
            self.pending_command = Some(text);
        }
        Ok(true)
    }

    // ── Menu navigation ───────────────────────────────────────

    async fn handle_menu_key(&mut self, key: crossterm::event::KeyEvent) -> Option<bool> {
        let is_up = key.code == KeyCode::Up
            || (key.code == KeyCode::Char('k') && key.modifiers.contains(KeyModifiers::CONTROL));
        let is_down = key.code == KeyCode::Down
            || key.code == KeyCode::Tab
            || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL));

        if is_up {
            self.menu_navigate(-1);
            return Some(true);
        }
        if is_down {
            self.menu_navigate(1);
            return Some(true);
        }

        match key.code {
            KeyCode::Enter => {
                self.handle_menu_select().await;
                return Some(true);
            }
            KeyCode::Esc => {
                self.menu = MenuContent::None;
                if matches!(self.prompt_mode, PromptMode::WizardInput { .. }) {
                    self.prompt_mode = PromptMode::Chat;
                    self.provider_wizard = None;
                    self.textarea.select_all();
                    self.textarea.cut();
                }
                return Some(true);
            }
            _ => {}
        }
        None
    }

    fn menu_navigate(&mut self, dir: i8) {
        macro_rules! nav {
            ($dd:expr) => {
                if dir < 0 { $dd.up() } else { $dd.down() }
            };
        }
        match &mut self.menu {
            MenuContent::Slash(dd) => nav!(dd),
            MenuContent::Model(dd) => nav!(dd),
            MenuContent::Provider(dd) => nav!(dd),
            MenuContent::Session(dd) => nav!(dd),
            MenuContent::File { dropdown: dd, .. } => nav!(dd),
            MenuContent::Approval { .. }
            | MenuContent::LoopCap
            | MenuContent::WizardTrail(_)
            | MenuContent::None => {}
        }
    }

    async fn handle_menu_select(&mut self) {
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
                        self.config
                            .query_and_apply_capabilities(prov.as_ref())
                            .await;
                    }
                    crate::tui_wizards::save_provider(&self.config);
                    self.scroll_buffer.push(Line::styled(
                        format!("  \u{2714} Model set to: {model_id}"),
                        Style::default().fg(Color::Green),
                    ));
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
                        let env_name = ptype.env_key_name().to_string();
                        let has_key = koda_core::runtime_env::is_set(&env_name);
                        let label = if has_key {
                            format!("API key for {} (Enter to keep current)", ptype)
                        } else {
                            format!("API key for {}", ptype)
                        };
                        self.menu =
                            MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
                        self.prompt_mode = PromptMode::WizardInput { label };
                        self.provider_wizard = Some(ProviderWizard::NeedApiKey {
                            provider_type: ptype,
                            base_url,
                            env_name,
                        });
                        self.textarea.select_all();
                        self.textarea.cut();
                    } else {
                        self.menu =
                            MenuContent::WizardTrail(vec![("Provider".into(), provider_name)]);
                        self.prompt_mode = PromptMode::WizardInput {
                            label: format!("{} URL", ptype),
                        };
                        self.provider_wizard = Some(ProviderWizard::NeedUrl {
                            provider_type: ptype,
                        });
                        self.textarea.select_all();
                        self.textarea.cut();
                        self.textarea.insert_str(&base_url);
                    }
                }
                return;
            }
            MenuContent::Session(dd) => {
                if let Some(item) = dd.selected_item() {
                    if item.is_current {
                        self.scroll_buffer.push(Line::styled(
                            "  Already in this session.",
                            Style::default().fg(Color::DarkGray),
                        ));
                    } else {
                        let target_id = item.id.clone();
                        let short = item.short_id.clone();
                        self.session.id = target_id;
                        self.scroll_buffer.push(Line::from(vec![
                            Span::styled("  \u{2714} ", Style::default().fg(Color::Green)),
                            Span::raw("Resumed session "),
                            Span::styled(short, Style::default().fg(Color::Cyan)),
                        ]));
                    }
                }
            }
            MenuContent::File { dropdown, prefix } => {
                if let Some(item) = dropdown.selected_item() {
                    let replacement = format!("{}@{}", prefix, item.path);
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
    }

    // ── Wizard submit ──────────────────────────────────────────

    async fn handle_wizard_submit(&mut self) {
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
                    if value.is_empty() && !koda_core::runtime_env::is_set(&env_name) {
                        self.scroll_buffer.push(Line::styled(
                            "  \u{2716} No API key provided.",
                            Style::default().fg(Color::Red),
                        ));
                        self.prompt_mode = PromptMode::Chat;
                        self.menu = MenuContent::None;
                        return;
                    }
                    if !value.is_empty() {
                        koda_core::runtime_env::set(&env_name, &value);
                        if let Ok(mut store) = koda_core::keystore::KeyStore::load() {
                            store.set(&env_name, &value);
                            let _ = store.save();
                        }
                        let masked = koda_core::keystore::mask_key(&value);
                        self.scroll_buffer.push(Line::styled(
                            format!("  \u{2714} {env_name} set to {masked}"),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    self.apply_provider(provider_type, base_url).await;
                }
                ProviderWizard::NeedUrl { provider_type } => {
                    let url = if value.is_empty() {
                        provider_type.default_base_url().to_string()
                    } else {
                        value
                    };
                    self.apply_provider(provider_type, url).await;
                }
            }
        }
        self.prompt_mode = PromptMode::Chat;
        self.menu = MenuContent::None;
    }

    async fn apply_provider(
        &mut self,
        provider_type: koda_core::config::ProviderType,
        base_url: String,
    ) {
        self.config.provider_type = provider_type.clone();
        self.config.base_url = base_url.clone();
        self.config.model = provider_type.default_model().to_string();
        self.config.model_settings.model = self.config.model.clone();
        self.config.recalculate_model_derived();
        *self.provider.write().await = koda_core::providers::create_provider(&self.config);
        crate::tui_wizards::save_provider(&self.config);

        let prov = self.provider.read().await;
        if let Ok(models) = prov.list_models().await {
            if let Some(first) = models.first() {
                self.config.model = first.id.clone();
                self.config.model_settings.model = self.config.model.clone();
                self.config.recalculate_model_derived();
            }
            self.config
                .query_and_apply_capabilities(prov.as_ref())
                .await;
            self.completer
                .set_model_names(models.iter().map(|m| m.id.clone()).collect());
        }
        self.renderer.model = self.config.model.clone();
        self.scroll_buffer.push(Line::styled(
            format!(
                "  \u{2714} Provider: {} ({})",
                self.config.provider_type, self.config.model
            ),
            Style::default().fg(Color::Green),
        ));
    }

    // ── History navigation ──────────────────────────────────────

    /// Terminal (width, height) for scroll math.
    fn term_dims(&self) -> (usize, usize) {
        let size = self.terminal.size().unwrap_or_default();
        (size.width as usize, size.height as usize)
    }

    // ── Mouse handling ────────────────────────────────────────

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crate::mouse_select::{Selection, VisualPos};
        use crossterm::event::{MouseButton, MouseEventKind};

        let (w, h) = self.term_dims();
        let hist_y = self.history_area_y;
        let hist_h = self.history_area_height;

        // Check if mouse is in the history area
        let in_history = mouse.row >= hist_y && mouse.row < hist_y + hist_h;

        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_buffer.scroll_up(3, w, h),
            MouseEventKind::ScrollDown => self.scroll_buffer.scroll_down(3),

            MouseEventKind::Down(MouseButton::Left) if in_history => {
                let row = mouse.row.saturating_sub(hist_y);
                self.mouse_selection = Some(Selection {
                    anchor: VisualPos {
                        row,
                        col: mouse.column,
                    },
                    cursor: VisualPos {
                        row,
                        col: mouse.column,
                    },
                });
            }

            MouseEventKind::Drag(MouseButton::Left) if in_history => {
                if let Some(sel) = &mut self.mouse_selection {
                    let row = mouse.row.saturating_sub(hist_y);
                    sel.cursor = VisualPos {
                        row,
                        col: mouse.column,
                    };
                }
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(sel) = self.mouse_selection.take() {
                    // Only copy if the selection spans more than a click
                    if sel.anchor != sel.cursor {
                        let lines: Vec<Line<'_>> =
                            self.scroll_buffer.all_lines().cloned().collect();
                        let scroll_pos = self.scroll_buffer.paragraph_scroll(hist_h as usize, w);
                        let visible = crate::mouse_select::extract_visible_text(
                            &lines,
                            scroll_pos.0,
                            w,
                            hist_h as usize,
                        );
                        let text = crate::mouse_select::extract_selected_text(&visible, &sel);
                        if !text.is_empty() {
                            match crate::mouse_select::copy_to_clipboard(&text) {
                                Ok(msg) => {
                                    self.scroll_buffer.push(Line::from(vec![
                                        Span::styled(
                                            "  \u{1f4cb} ",
                                            Style::default().fg(Color::Green),
                                        ),
                                        Span::styled(msg, Style::default().fg(Color::Green)),
                                    ]));
                                }
                                Err(e) => {
                                    tracing::warn!("clipboard copy failed: {e}");
                                }
                            }
                        }
                    }
                }
            }

            _ => {}
        }
    }

    // ── Clipboard ─────────────────────────────────────────────

    fn copy_to_clipboard(&mut self, shift: bool) {
        let text = if shift {
            self.scroll_buffer.last_response()
        } else {
            self.scroll_buffer.last_code_block()
        };
        match text {
            Some(content) => {
                match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&content)) {
                    Ok(()) => {
                        let label = if shift { "response" } else { "code block" };
                        let preview: String = content.chars().take(60).collect();
                        self.scroll_buffer.push(Line::from(vec![
                            Span::styled("  \u{1f4cb} ", Style::default().fg(Color::Green)),
                            Span::styled(
                                format!("Copied {label} to clipboard"),
                                Style::default().fg(Color::Green),
                            ),
                            Span::styled(
                                format!(" ({preview}…)"),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                    Err(e) => {
                        self.scroll_buffer.push(Line::styled(
                            format!("  \u{2717} Clipboard error: {e}"),
                            Style::default().fg(Color::Red),
                        ));
                    }
                }
            }
            None => {
                let label = if shift {
                    "No response to copy."
                } else {
                    "No code block to copy."
                };
                self.scroll_buffer.push(Line::styled(
                    format!("  {label}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }

    // ── History ────────────────────────────────────────────────

    fn history_up(&mut self) {
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

    fn history_down(&mut self) {
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

    // ── Reactive menu updates ───────────────────────────────────

    fn update_reactive_menu(&mut self) {
        let after_input = self.textarea.lines().join("\n");
        let trimmed = after_input.trim_end();

        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            if let Some(dd) =
                crate::widgets::slash_menu::from_input(crate::completer::SLASH_COMMANDS, trimmed)
            {
                self.menu = MenuContent::Slash(dd);
            } else if matches!(self.menu, MenuContent::Slash(_)) {
                self.menu = MenuContent::None;
            }
        } else if let Some(at_pos) = crate::completer::find_last_at_token(trimmed) {
            let partial = &trimmed[at_pos + 1..];
            let prefix = &trimmed[..at_pos];
            let matches = crate::completer::list_path_matches_public(&self.project_root, partial);
            if !matches.is_empty() {
                let items: Vec<crate::widgets::file_menu::FileItem> = matches
                    .iter()
                    .map(|p| crate::widgets::file_menu::FileItem {
                        path: p.clone(),
                        is_dir: p.ends_with('/'),
                    })
                    .collect();
                let dd = crate::widgets::dropdown::DropdownState::new(items, "\u{1f4c2} Files");
                self.menu = MenuContent::File {
                    dropdown: dd,
                    prefix: prefix.to_string(),
                };
            } else if matches!(self.menu, MenuContent::File { .. }) {
                self.menu = MenuContent::None;
            }
        } else if matches!(self.menu, MenuContent::Slash(_) | MenuContent::File { .. }) {
            self.menu = MenuContent::None;
        }
    }
}

// ---------------------------------------------------------------------------
// Command history persistence
// ---------------------------------------------------------------------------

const MAX_HISTORY: usize = 500;

fn history_file_path() -> std::path::PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(config_dir)
        .join("koda")
        .join("history")
}

fn load_history() -> Vec<String> {
    let path = history_file_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub(crate) fn save_history(history: &[String]) {
    let path = history_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let start = history.len().saturating_sub(MAX_HISTORY);
    let content = history[start..].join("\n");
    let _ = std::fs::write(&path, content);
}
