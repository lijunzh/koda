//! TUI shared context — the mutable state struct for the event loop.
//!
//! ## Module layout
//!
//! - **mod.rs** — struct definition, constructor, lifecycle, command dispatch
//! - **events.rs** — keyboard, mouse, paste, clipboard, history input handling
//! - **menus.rs** — dropdown pickers, menu navigation, wizard state machines

mod events;
mod menus;

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
use koda_core::config::KodaConfig;
use koda_core::db::Role;
use koda_core::engine::EngineCommand;
use koda_core::persistence::Persistence;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use koda_core::trust::{self, TrustMode};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;

use crate::composer::keymap::RuntimeKeymap;
use crate::composer::textarea::TextArea;

/// All mutable TUI state, extracted from `run()`'s local variables.
///
/// Methods are split across submodules by concern:
/// - **mod.rs** — lifecycle (new, draw, cleanup, event loop, dispatch)
/// - **events.rs** — keyboard, mouse, paste, clipboard, history
/// - **menus.rs** — dropdown pickers, menu navigation, wizards
pub(crate) struct TuiContext {
    // ── UI state ────────────────────────────────────────────
    pub terminal: Term,
    pub textarea: TextArea,
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
    /// Inputs queued during inference for injection into the **next** turn
    /// after the current one completes.  All pending items are joined with
    /// `\n\n` and submitted as a single new turn ("later" lane).
    ///
    /// Mid-turn injection ("next" lane) is handled by sending
    /// `EngineCommand::QueueNext` directly to the engine via `cmd_tx`.
    pub later_queue: VecDeque<String>,
    pub pending_command: Option<String>,
    pub should_quit: bool,
    pub silent_compact_deferred: bool,
    pub inference_start: Option<std::time::Instant>,
    /// Context window usage percentage (0-100), updated via EngineEvent::ContextUsage.
    pub context_pct: u32,
    pub history: Vec<String>,
    pub history_idx: Option<usize>,
    pub completer: crate::completer::InputCompleter,
    pub mouse_selection: Option<crate::mouse_select::Selection>,
    /// Y-coordinate of the history area top edge (for mouse mapping).
    pub history_area_y: u16,
    /// Height of the history area in rows.
    pub history_area_height: u16,

    /// Non-bracketed paste-burst detector (#1186). Buffers fast-typed
    /// runs that look like a paste from a terminal that doesn't emit
    /// bracketed-paste sequences (older terminals, some SSH setups,
    /// tmux without `set -g set-clipboard on`). When the burst flushes
    /// (via the idle-loop timer in [`TuiContext::run`]), the accumulated
    /// text is dropped into the textarea as a single insert — cheaper
    /// than re-rendering after every fast keystroke and avoids paste
    /// content being interpreted as a sequence of submits when newlines
    /// are involved.
    pub paste_burst: crate::composer::paste_burst::PasteBurst,

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
    pub shared_mode: trust::SharedTrustMode,
    pub agent: Arc<KodaAgent>,
    pub project_root: PathBuf,

    /// Coalescing draw scheduler (#1138). Replaces the old per-iteration
    /// synchronous `terminal.draw()` calls in the inference hot loop.
    /// `frame_requester` is the producer side; clone freely. `draw_rx` is
    /// the consumer side and lives on the inference loop's select.
    pub frame_requester: crate::frame_requester::FrameRequester,
    pub draw_rx: crate::frame_requester::DrawNotifyRx,

    /// Live activity tracker for bg agents + processes (#1210).
    /// Absorbs `BgChildActivity` / `BgTaskUpdate` events into a per-task
    /// last-seen map; projected each frame into the
    /// `widgets::bg_activity_overlay` rendered above the status bar.
    /// See [`crate::bg_activity::BgActivityTracker`].
    pub bg_activity: crate::bg_activity::BgActivityTracker,
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

/// Resolve the initial trust mode for a session, given any persisted mode
/// string and a fallback to use when nothing valid is persisted.
///
/// Resume semantics (#590): a previously-stored mode wins over the CLI/config
/// value, so users don't lose their per-session preference on `/resume`.
///
/// Fallback semantics (#982): when there's no persisted mode (fresh session)
/// OR the persisted string is unparseable (corruption / older format), fall
/// back to the user's current `config.trust` — **never** to a hardcoded
/// constant. Hardcoding `Auto` here was the original bug: it silently
/// overrode `--mode safe`, defeating the whole point of the flag.
pub(crate) fn resolve_initial_trust_mode(
    persisted: Option<&str>,
    fallback: TrustMode,
) -> TrustMode {
    persisted.and_then(TrustMode::parse).unwrap_or(fallback)
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
        // Restore last-used provider from DB (#693)
        if let Ok(Some(last)) = koda_core::last_provider::load_last_provider(&db).await {
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
        startup_lines.extend(crate::startup::home_dir_warning_lines(&project_root));

        if let Ok(Some(latest)) = version_check.await
            && let Some((current, latest)) = koda_core::version::update_available(&latest)
        {
            startup_lines.extend(crate::startup::update_notice_lines(current, &latest));
        }

        // Extract (name, description) pairs from SLASH_COMMANDS for the system prompt.
        let commands: Vec<(&str, &str)> = crate::completer::SLASH_COMMANDS
            .iter()
            .map(|&(name, desc, _)| (name, desc))
            .collect();

        let mut agent =
            koda_core::agent::KodaAgent::new(&config, project_root.clone(), &commands).await?;
        crate::builtin_skills::inject_builtin_skills(&mut agent);
        agent.rebuild_system_prompt(&config, &commands);
        let agent = Arc::new(agent);

        let mut session =
            KodaSession::new(session_id, agent.clone(), db.clone(), &config, config.trust).await;

        crate::startup::purge_nudge(&db, &mut startup_lines).await;

        // Restore persisted approval mode on resume (#590).
        // Fallback to the user's current CLI/config trust mode (not a hardcoded
        // constant) so `--mode safe` is honored on first run when there's no
        // persisted mode yet. Fixes #982.
        let initial_mode = {
            use koda_core::persistence::Persistence;
            let persisted = session
                .db
                .get_session_mode(&session.id)
                .await
                .ok()
                .flatten();
            resolve_initial_trust_mode(persisted.as_deref(), config.trust)
        };
        let shared_mode = trust::new_shared_trust(initial_mode);

        // Terminal + textarea
        let terminal = init_terminal()?;

        // Composer textarea (codex `bottom_pane::textarea` port; #1178 PR 2).
        // No cursor/style/placeholder configuration here — codex's textarea
        // owns its own cursor styling (block in vim normal, bar in insert)
        // and koda renders the empty-state "Type a message..." hint outside
        // the textarea in `tui_viewport::draw_viewport` so we don't depend on
        // ratatui-textarea's placeholder feature (codex doesn't have an
        // equivalent at the textarea level).
        //
        // Keymap (PR 3): construct defaults and hand them to the textarea so
        // its internal `input(key)` dispatcher routes to the editor / vim /
        // vim-operator handlers correctly. The textarea owns its own clone of
        // the keymap from this point on; TuiContext does not need to keep a
        // copy. Future PRs that need the keymap (e.g. the key-hint footer)
        // can either ask the textarea for it or rebuild defaults on the fly.
        let mut textarea = TextArea::new();
        textarea.set_keymap_bindings(&RuntimeKeymap::defaults());

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
            let providers = menus::PROVIDERS;
            let items: Vec<crate::widgets::provider_menu::ProviderItem> = providers
                .iter()
                .map(|(key, name)| {
                    let ptype = koda_core::config::ProviderType::from_url_or_name("", Some(key));
                    crate::widgets::provider_menu::ProviderItem {
                        key,
                        name,
                        local: !ptype.requires_api_key(),
                        key_set: false,
                    }
                })
                .collect();
            menu = MenuContent::Provider(crate::widgets::dropdown::DropdownState::new(
                items,
                "\u{1f43b} Choose your LLM provider",
            ));
        }

        // Load and render historical conversation on session resume
        let (history_lines, oldest_msg_id, interruption, away_banner) = {
            use koda_core::db::queries::detect_interruption;
            use koda_core::persistence::{Persistence, Role};
            // Read idle time BEFORE load_context updates last_accessed_at.
            let idle_secs = session
                .db
                .get_session_idle_secs(&session.id)
                .await
                .ok()
                .flatten();
            match session.db.load_context(&session.id).await {
                Ok(msgs) if !msgs.is_empty() => {
                    session.title_set = true; // resumed session already has a title
                    let oldest_id = msgs.first().map(|m| m.id);
                    let interrupted = detect_interruption(&msgs);
                    // Compute stats for the away-summary banner.
                    let user_msgs = msgs.iter().filter(|m| m.role == Role::User).count();
                    let tool_calls = msgs.iter().filter(|m| m.role == Role::Tool).count();
                    let total_tokens: i64 = msgs
                        .iter()
                        .map(|m| m.prompt_tokens.unwrap_or(0) + m.completion_tokens.unwrap_or(0))
                        .sum();
                    let banner = crate::tui_output::away_summary_banner(
                        idle_secs,
                        None, // title visible in startup hint; keep banner slim
                        user_msgs,
                        tool_calls,
                        total_tokens,
                    );
                    let lines = crate::history_render::render_history_messages(&msgs);
                    (lines, oldest_id, interrupted, banner)
                }
                _ => (Vec::new(), None, None, Vec::new()),
            }
        };

        // Spawn the coalescing draw scheduler (#1138). The inference loop's
        // select consumes from `draw_rx`; everywhere else holds clones of
        // `frame_requester` to fire-and-forget redraw requests.
        let (frame_requester, draw_rx) = crate::frame_requester::spawn_frame_scheduler();

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
                if !away_banner.is_empty() {
                    buf.push_lines(away_banner);
                }
                if let Some(kind) = &interruption {
                    buf.push_lines(crate::tui_output::interrupted_turn_banner(kind));
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
            later_queue: VecDeque::new(),
            pending_command: None,
            should_quit: false,
            silent_compact_deferred: false,
            inference_start: None,
            context_pct: 0,
            history: db.history_load().await.unwrap_or_default(),
            history_idx: None,
            completer,
            mouse_selection: None,
            history_area_y: 0,
            history_area_height: 0,
            paste_burst: crate::composer::paste_burst::PasteBurst::default(),
            config,
            provider,
            session,
            shared_mode,
            agent,
            project_root,
            frame_requester,
            draw_rx,
            bg_activity: crate::bg_activity::BgActivityTracker::default(),
        })
    }

    /// Draw the fullscreen viewport.
    pub fn draw(&mut self) -> Result<()> {
        // Clamp scroll offset to valid bounds before rendering.
        // Necessary after terminal resize changes wrapping math.
        // Use history panel height, not full terminal height — otherwise
        // max_offset is too small and scrolling up re-engages sticky.
        let (w, _) = self.term_dims();
        let vh = (self.history_area_height as usize).max(1);
        self.scroll_buffer.clamp_offset(w, vh);

        let mode = trust::read_trust(&self.shared_mode);
        let ctx = self.context_pct;
        let tui_state = self.tui_state;
        let prompt_mode = &self.prompt_mode;
        let queue_total = self.later_queue.len();
        // Collect at most MAX_VISIBLE preview items *before* the terminal.draw
        // borrow so we don't fight the borrow checker on &mut self.terminal.
        let queue_preview: Vec<String> = self
            .later_queue
            .iter()
            .take(crate::widgets::queue_preview::MAX_VISIBLE)
            .cloned()
            .collect();
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
        let mcp_info = self.agent.mcp_status_bar_info();
        let project_root = self.project_root.clone();
        // #1210: project bg-agent + bg-process snapshots through the
        // activity tracker into renderable rows for the live overlay
        // above the status bar. Cheap — snapshots are a vec clone
        // under one short lock; tracker projection is O(n) over the
        // (always small: bounded by sub-agent fan-out cap) snapshot
        // size. Done before the terminal.draw closure to satisfy the
        // borrow checker around `&mut self`.
        //
        // Replaces the #1158 ambient `bg_counts` status pill: the
        // overlay shows what's running, what each task is doing right
        // now, and the cancel keybindings — the pill's job becomes
        // redundant the moment the live surface exists (#1210).
        let agent_snaps = self.session.bg_agents.snapshot();
        let process_snaps = self.agent.tools.bg_registry.snapshot();
        let (bg_activity_rows, bg_activity_total) = self
            .bg_activity
            .build_rows(&agent_snaps, &process_snaps);

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
                &queue_preview,
                queue_total,
                elapsed,
                last_turn,
                menu,
                scroll_buffer,
                selection,
                mcp_info,
                &project_root,
                &bg_activity_rows,
                bg_activity_total,
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
            //
            // The optional paste-burst flush arm (#1186) only races the
            // event branch when a non-bracketed paste is currently being
            // accumulated. The `if` guard makes the branch disabled when
            // there's no pending burst, so we don't churn timers in the
            // common case (idle waiting for the user). When a burst IS
            // active and the user stops typing, the timer fires and we
            // commit the buffered text into the textarea as one insert.
            tokio::select! {
                Some(Ok(ev)) = self.crossterm_events.next() => {
                    self.handle_idle_event(ev).await?;
                }
                _ = tokio::time::sleep(
                    crate::composer::paste_burst::PasteBurst::recommended_flush_delay(),
                ), if self.paste_burst.is_active() => {
                    self.flush_paste_burst_if_due();
                }
            }
        }
        Ok(())
    }

    // ═══════════════════════════════════════════════════════════
    // Command dispatch (slash commands, dropdown openers, inference prep)
    // ═══════════════════════════════════════════════════════════

    /// Dequeue the next pending input, if any.
    ///
    /// Priority order:
    /// 1. `pending_command` — set by slash commands that need a follow-up prompt
    /// 2. `later_queue` — all items are joined with `\n\n` and returned as one
    ///    batched string so they become a single new inference turn.
    fn dequeue_input(&mut self) -> Option<String> {
        if let Some(cmd) = self.pending_command.take() {
            return Some(cmd);
        }
        if let Some(first) = self.later_queue.front().cloned() {
            // Drain the whole later_queue and batch into one turn.
            let (joined, count) = crate::queue_lanes::drain_later_as_batch(&mut self.later_queue)
                .expect("front() returned Some, drain must too");
            let mode = trust::read_trust(&self.shared_mode);
            let icon = match mode {
                TrustMode::Plan => "\u{1f4cb}",
                TrustMode::Safe => "\u{1f512}",
                TrustMode::Auto => "\u{26a1}",
            };
            self.scroll_buffer.push(Line::from(vec![
                Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                Span::raw(first),
                if count > 1 {
                    Span::styled(
                        format!(" (+{} batched)", count - 1),
                        Style::default().fg(Color::DarkGray),
                    )
                } else {
                    Span::raw("")
                },
            ]));
            return Some(joined);
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
        if input.trim() == "/vim" {
            // Toggle vim-mode editing on the textarea (PR 3 of #1178).
            // The textarea owns the mode flag; we just flip it and emit
            // a confirmation line so the user sees the change in scrollback.
            // The status-bar `VIM:NORMAL` / `VIM:INSERT` pill (rendered in
            // `tui_viewport::draw_viewport`) reflects the current mode on
            // the next frame.
            let now_on = !self.textarea.is_vim_enabled();
            self.textarea.set_vim_enabled(now_on);
            let msg = if now_on {
                "\u{1f43b} Vim mode: ON \u{2014} Esc \u{2192} NORMAL, i/a/o \u{2192} INSERT."
            } else {
                "\u{1f43b} Vim mode: OFF."
            };
            self.scroll_buffer.push(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::Cyan),
            )));
            return CommandOutcome::Handled;
        }
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
            &mut self.bg_activity,
        )
        .await;

        match action {
            SlashAction::Continue => {
                self.reinit_after_slash().await;
                CommandOutcome::Handled
            }
            SlashAction::OpenKeyMenu => {
                self.open_key_picker().await;
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

        // Auto-set session title from first user message (#590)
        if !self.session.title_set {
            self.session.title_set = true;
            let title: String = user_message.chars().take(50).collect();
            let _ = self
                .session
                .db
                .set_session_title(&self.session.id, &title)
                .await;
        }

        let pending_images = if processed.images.is_empty() {
            None
        } else {
            Some(processed.images)
        };

        self.session.mode = trust::read_trust(&self.shared_mode);
        self.session.update_provider(&self.config);

        CommandOutcome::StartInference { pending_images }
    }

    // ═══════════════════════════════════════════════════════════
    // Idle-mode event handling (keyboard, paste, menu, wizard, history)
    // ═══════════════════════════════════════════════════════════
}

#[cfg(test)]
mod tests {
    //! Regression tests for #982: `--mode safe` was silently overridden by a
    //! hardcoded `TrustMode::Auto` at session-creation time. The fix routes
    //! `config.trust` through both fresh-session and resume-fallback paths.
    //!
    //! These tests pin the resolution semantics so a future refactor can't
    //! reintroduce a hardcoded constant without turning the build red.

    use super::resolve_initial_trust_mode;
    use koda_core::trust::TrustMode;

    #[test]
    fn fresh_session_no_persisted_mode_uses_config_trust() {
        // The exact bug: fresh session, no persisted mode, user passed `--mode safe`.
        // Pre-fix this returned hardcoded `Auto`, ignoring the user's choice.
        assert_eq!(
            resolve_initial_trust_mode(None, TrustMode::Safe),
            TrustMode::Safe,
        );
        assert_eq!(
            resolve_initial_trust_mode(None, TrustMode::Plan),
            TrustMode::Plan,
        );
        assert_eq!(
            resolve_initial_trust_mode(None, TrustMode::Auto),
            TrustMode::Auto,
        );
    }

    #[test]
    fn persisted_mode_wins_over_config_trust_on_resume() {
        // Resume semantics from #590: a stored mode beats the CLI fallback.
        assert_eq!(
            resolve_initial_trust_mode(Some("plan"), TrustMode::Auto),
            TrustMode::Plan,
        );
        assert_eq!(
            resolve_initial_trust_mode(Some("safe"), TrustMode::Auto),
            TrustMode::Safe,
        );
        assert_eq!(
            resolve_initial_trust_mode(Some("auto"), TrustMode::Safe),
            TrustMode::Auto,
        );
    }

    #[test]
    fn unparseable_persisted_mode_falls_back_to_config_trust_not_auto() {
        // The other half of the bug: corrupt/unknown persisted strings used
        // to fall through to a hardcoded `Auto`. Now they honor config.trust.
        assert_eq!(
            resolve_initial_trust_mode(Some("bogus"), TrustMode::Safe),
            TrustMode::Safe,
        );
        assert_eq!(
            resolve_initial_trust_mode(Some(""), TrustMode::Plan),
            TrustMode::Plan,
        );
    }

    #[test]
    fn parse_accepts_legacy_aliases_for_persisted_strings() {
        // `TrustMode::parse` accepts aliases like "yolo", "strict", etc.
        // Make sure resume of older sessions stored under those names still works.
        assert_eq!(
            resolve_initial_trust_mode(Some("yolo"), TrustMode::Safe),
            TrustMode::Auto,
        );
        assert_eq!(
            resolve_initial_trust_mode(Some("strict"), TrustMode::Auto),
            TrustMode::Safe,
        );
        assert_eq!(
            resolve_initial_trust_mode(Some("readonly"), TrustMode::Auto),
            TrustMode::Plan,
        );
    }
}
