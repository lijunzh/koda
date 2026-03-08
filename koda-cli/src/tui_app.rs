//! TUI-based interactive event loop with persistent inline viewport.
//!
//! # Rendering Architecture
//!
//! Two rendering systems coexist, bridged by terminal re-initialization:
//!
//! ## 1. Engine output — ratatui `insert_before()`
//!
//! LLM streaming, tool calls, diffs, and approval prompts render as
//! native `ratatui::text::Line`/`Span` via `tui_output::emit_line()`.
//! Content scrolls into terminal scrollback above the viewport.
//!
//! ## 2. Slash commands — crossterm direct writes
//!
//! `/model`, `/help`, `/provider`, etc. render via `tui_output::write_line()`
//! which writes styled content directly to stdout with `\r\n` line endings.
//! Interactive select menus (`select_inline`) use crossterm cursor movement
//! for in-place arrow-key navigation.
//!
//! ## Bridge: `init_terminal()` resync
//!
//! After every slash command, we create a fresh `Terminal` with
//! `Viewport::Inline(2)`. This anchors the viewport at the current
//! cursor position — wherever crossterm left it — eliminating stale
//! cursor tracking. No raw mode toggle needed.
//!
//! ```text
//! ┌─ scrollback ────────────────────────────────────────────┐
//! │ Engine output via insert_before() (ratatui Line/Span)   │
//! │ Slash command output via write_line() (crossterm)       │
//! │ Select menus via select_inline() (crossterm)            │
//! ├─────────────────────────────────────────────────────────┤
//! │ ← init_terminal() resyncs viewport here →              │
//! ├─ ratatui Viewport::Inline(2) ──────────────────────────┤
//! │ \u{1f43b}> user types here even during inference_              │
//! │ model │ normal │ ████░░ 5%                             │
//! └─────────────────────────────────────────────────────────┘
//! ```

use crate::input;
use crate::sink::UiEvent;
use crate::tui_commands::{self, SlashAction};
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use crate::widgets::status_bar::StatusBar;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

/// Minimum viewport height (1 top separator + 1 input + 1 bottom separator + 1 status bar).
const MIN_VIEWPORT_HEIGHT: u16 = 4;
/// Maximum viewport height to avoid taking over the terminal.
const MAX_VIEWPORT_HEIGHT: u16 = 10;

// ── Session state ────────────────────────────────────────────

/// What the TUI is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiState {
    /// Waiting for user input (no inference running).
    Idle,
    /// An inference turn is running.
    Inferring,
}

// ── Viewport drawing ─────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_viewport(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    model: &str,
    tier_label: &str,
    mode: ApprovalMode,
    context_pct: u32,
    state: TuiState,
    queue_len: usize,
    elapsed_secs: u64,
    last_turn: Option<&crate::widgets::status_bar::TurnStats>,
    slash_menu: Option<&crate::widgets::slash_menu::SlashMenuState>,
) {
    let area = frame.area();
    let menu_height: u16 = slash_menu.map_or(0, |m| m.height());
    let [menu_area, sep_row, input_rows, bot_sep_row, status_row] = Layout::vertical([
        Constraint::Length(menu_height),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // Separator line: ──────────── 🐻 ─
    let sep_width = sep_row.width.saturating_sub(5) as usize; // 5 = " 🐻 ─"
    let separator = Line::from(vec![
        Span::styled(
            "─".repeat(sep_width),
            Style::default().fg(Color::Rgb(124, 111, 100)), // WARM_MUTED
        ),
        Span::styled(" 🐻 ─", Style::default().fg(Color::Rgb(124, 111, 100))),
    ]);
    frame.render_widget(separator, sep_row);

    // Slash command menu (shown when input starts with /)
    if let Some(menu) = slash_menu {
        let menu_lines = crate::widgets::slash_menu::build_menu_lines(menu);
        let menu_widget = Paragraph::new(menu_lines);
        frame.render_widget(menu_widget, menu_area);
    }

    // Prompt icon + textarea
    let (icon, color) = match (state, mode) {
        (TuiState::Inferring, _) => ("\u{23f3}", Color::DarkGray), // ⏳ during inference
        (_, ApprovalMode::Safe) => ("🔍", Color::Yellow),
        (_, ApprovalMode::Strict) => ("🔒", Color::Cyan),
        (_, ApprovalMode::Auto) => ("⚡", Color::Green),
    };
    let prompt_width: u16 = 4;
    let [prompt_area, text_area] =
        Layout::horizontal([Constraint::Length(prompt_width), Constraint::Fill(1)])
            .areas(input_rows);

    frame.render_widget(
        Paragraph::new(format!("{icon}> ")).style(Style::default().fg(color)),
        prompt_area,
    );
    frame.render_widget(textarea, text_area);

    // Bottom separator between input and status bar
    let bot_width = bot_sep_row.width as usize;
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(bot_width),
            Style::default().fg(Color::Rgb(124, 111, 100)),
        ))),
        bot_sep_row,
    );

    // Status bar
    let mut sb = StatusBar::new(model, tier_label, mode.label(), context_pct);
    if queue_len > 0 {
        sb = sb.with_queue(queue_len);
    }
    if elapsed_secs > 0 {
        sb = sb.with_elapsed(elapsed_secs);
    }
    if let Some(stats) = last_turn {
        sb = sb.with_last_turn(stats);
    }
    frame.render_widget(sb, status_row);
}

// ── Terminal lifecycle helpers ────────────────────────────────

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn init_terminal(height: u16) -> Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    // Flush pending output before the cursor-position query
    // that Viewport::Inline triggers internally.
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Retry up to 3 times — the DSR cursor-position query can
    // time out if a prior EventStream wake thread is still draining.
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let stdout = std::io::stdout();
        let backend = CrosstermBackend::new(stdout);
        match Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        ) {
            Ok(t) => return Ok(t),
            Err(e) => {
                tracing::debug!("init_terminal attempt {}: {e}", attempt + 1);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap().into())
}

fn restore_terminal(terminal: &mut Term, height: u16) {
    let _ = terminal.clear();
    let _ = crossterm::terminal::disable_raw_mode();
    // Erase leftover viewport lines
    print!("\x1b[{}A\x1b[J", height);
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

/// Reinitialize the viewport with a new height.
///
/// Drops the old terminal, erases the stale viewport area using the
/// **old** height (to avoid overshooting into scrollback), and creates
/// a fresh terminal with the new height.
fn reinit_viewport(terminal: Term, old_height: u16, new_height: u16) -> Result<Term> {
    drop(terminal);
    let _ = crossterm::terminal::disable_raw_mode();
    // Move cursor up past the OLD viewport and erase to end of screen
    print!("\x1b[{}A\x1b[J", old_height);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    init_terminal(new_height)
}

/// Resize the viewport if the textarea line count changed.
///
/// Returns the (possibly new) terminal and updated height.
/// Reinitializes the terminal when the viewport needs to grow or shrink.
fn maybe_resize_viewport(
    terminal: Term,
    textarea: &TextArea,
    current_height: u16,
    extra_height: u16, // slash menu, etc.
) -> Result<(Term, u16)> {
    let input_lines = textarea.lines().len().max(1) as u16;
    let base = (input_lines + 1).clamp(MIN_VIEWPORT_HEIGHT, MAX_VIEWPORT_HEIGHT);
    let desired = base + extra_height;
    if desired == current_height {
        return Ok((terminal, current_height));
    }
    let new_term = reinit_viewport(terminal, current_height, desired)?;
    Ok((new_term, desired))
}

// ── Output helper ────────────────────────────────────────────

/// Write a message line above the viewport.
fn emit_above(terminal: &mut Term, line: ratatui::text::Line<'_>) {
    crate::tui_output::emit_line(terminal, line);
}

// ── Main event loop ──────────────────────────────────────────

/// Run the main interactive event loop with persistent TUI.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
) -> Result<()> {
    // ── Setup (same as before) ───────────────────────────────

    let settings = koda_core::approval::Settings::load();
    if let Some(ref last) = settings.last_provider {
        let ptype =
            koda_core::config::ProviderType::from_url_or_name("", Some(&last.provider_type));
        config.provider_type = ptype;
        config.base_url = last.base_url.clone();
        config.model = last.model.clone();
        config.model_settings.model = last.model.clone();
        // Recalculate context window and tier for the restored model
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

    // Query actual model capabilities from the provider API.
    // This overrides the hardcoded context window with the real value.
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

    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);
    crate::startup::print_mcp_status(&agent.mcp_statuses);

    let mut session = KodaSession::new(
        session_id.clone(),
        agent.clone(),
        db,
        &config,
        ApprovalMode::Auto,
    );

    let shared_mode = approval::new_shared_mode(ApprovalMode::Auto);

    // ── Initialize persistent terminal ───────────────────────

    let mut viewport_height = MIN_VIEWPORT_HEIGHT;
    let mut terminal = init_terminal(viewport_height)?;

    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::REVERSED),
    );
    textarea.set_placeholder_text("Type a message...");
    textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));

    // ── Channels ─────────────────────────────────────────────

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);

    // ── State ────────────────────────────────────────────────

    let mut renderer = TuiRenderer::new();
    renderer.model = config.model.clone();
    let mut tui_state = TuiState::Idle;
    let mut input_queue: VecDeque<String> = VecDeque::new();
    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;
    let mut should_quit = false;
    let mut slash_menu: Option<crate::widgets::slash_menu::SlashMenuState> = None;
    let mut inference_start: Option<std::time::Instant> = None;
    let mut history: Vec<String> = load_history();
    let mut history_idx: Option<usize> = None; // None = not browsing history
    let mut completer = crate::completer::InputCompleter::new(project_root.clone());

    // Cache model names for /model Tab completion
    {
        let prov = provider.read().await;
        if let Ok(models) = prov.list_models().await {
            completer.set_model_names(models.iter().map(|m| m.id.clone()).collect());
        }
    }

    // Crossterm event stream for async key capture
    let mut crossterm_events = EventStream::new();

    // ── Initial viewport draw ────────────────────────────────

    let mode = approval::read_mode(&shared_mode);
    let ctx = koda_core::context::percentage() as u32;
    (terminal, viewport_height) = maybe_resize_viewport(terminal, &textarea, viewport_height, 0)?;
    terminal.draw(|f| {
        draw_viewport(
            f,
            &textarea,
            &config.model,
            config.model_tier.label(),
            mode,
            ctx,
            tui_state,
            input_queue.len(),
            inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            renderer.last_turn_stats.as_ref(),
            slash_menu.as_ref(),
        );
    })?;

    // ── Main event loop ──────────────────────────────────────

    loop {
        if should_quit {
            break;
        }

        // Check if we have a queued or pending command to process
        if tui_state == TuiState::Idle {
            let input = if let Some(cmd) = pending_command.take() {
                Some(cmd)
            } else if let Some(queued) = input_queue.pop_front() {
                // Echo queued input above viewport
                let mode = approval::read_mode(&shared_mode);
                let icon = match mode {
                    ApprovalMode::Safe => "🔍",
                    ApprovalMode::Strict => "🔒",
                    ApprovalMode::Auto => "⚡",
                };
                emit_above(
                    &mut terminal,
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
                        let action = tui_commands::handle_slash_command(
                            &mut terminal,
                            &input,
                            &mut config,
                            &provider,
                            &mut session,
                            &shared_mode,
                            &mut renderer,
                            &project_root,
                            &agent,
                            &mut pending_command,
                        )
                        .await;

                        match action {
                            SlashAction::Continue => {
                                // Re-init terminal to resync viewport with cursor
                                // position after crossterm direct writes.
                                viewport_height = MIN_VIEWPORT_HEIGHT;
                                // Drop the old EventStream BEFORE init_terminal.
                                // EventStream spawns a background wake thread that
                                // reads from stdin; if it's still active it can
                                // consume the DSR response that Viewport::Inline's
                                // cursor-position query needs, causing a timeout.
                                crossterm_events = EventStream::new();
                                terminal = init_terminal(viewport_height)?;
                                // Refresh model name cache (provider may have changed)
                                let prov = provider.read().await;
                                if let Ok(models) = prov.list_models().await {
                                    completer.set_model_names(
                                        models.iter().map(|m| m.id.clone()).collect(),
                                    );
                                }
                                // Sync model name for cost estimation
                                renderer.model = config.model.clone();
                            }
                            SlashAction::Quit => {
                                tui_output::emit_line(
                                    &mut terminal,
                                    Line::styled(
                                        "\u{1f43b} Goodbye!",
                                        Style::default().fg(Color::Cyan),
                                    ),
                                );
                                should_quit = true;
                                continue;
                            }
                        }
                    } else {
                        // ── Start inference turn inline ──────────
                        let user_input = input.clone();
                        let processed = input::process_input(&user_input, &project_root);
                        if !processed.images.is_empty() {
                            for (i, _img) in processed.images.iter().enumerate() {
                                emit_above(
                                    &mut terminal,
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
                                    &mut terminal,
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

                        if let Err(e) = session
                            .db
                            .insert_message(
                                &session.id,
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

                        session.mode = approval::read_mode(&shared_mode);
                        session.update_provider(&config);

                        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());
                        let cancel_token = session.cancel.clone();

                        // Run the inference turn as a pinned future
                        tui_state = TuiState::Inferring;
                        inference_start = Some(std::time::Instant::now());
                        renderer.last_turn_stats = None;

                        {
                            let turn =
                                session.run_turn(&config, pending_images, &cli_sink, &mut cmd_rx);
                            tokio::pin!(turn);

                            loop {
                                // Redraw viewport inside inference loop
                                let mode = approval::read_mode(&shared_mode);
                                let ctx = koda_core::context::percentage() as u32;
                                terminal.draw(|f| {
                                    draw_viewport(
                                        f,
                                        &textarea,
                                        &config.model,
                                        config.model_tier.label(),
                                        mode,
                                        ctx,
                                        tui_state,
                                        input_queue.len(),
                                        inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
                                        renderer.last_turn_stats.as_ref(),
                                        slash_menu.as_ref(),
                                    );
                                })?;

                                tokio::select! {
                                    result = &mut turn => {
                                        if let Err(e) = result {
                                            emit_above(
                                                &mut terminal,
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
                                    Some(Ok(ev)) = crossterm_events.next() => {
                                        if let Event::Resize(_, _) = ev {
                                            // Terminal resized during inference — erase stale
                                            // viewport and reinit to prevent ghost prompt lines.
                                            terminal = reinit_viewport(terminal, viewport_height, viewport_height)?;
                                        } else if let Event::Key(key) = ev {
                                            match (key.code, key.modifiers) {
                                                (KeyCode::Enter, KeyModifiers::NONE) => {
                                                    let text = textarea.lines().join("\n");
                                                    if !text.trim().is_empty() {
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        history.push(text.clone());
                                                        save_history(&history);
                                                        history_idx = None;
                                                        input_queue.push_back(text);
                                                    }
                                                }
                                                (KeyCode::Esc, _) => {
                                                    cancel_token.cancel();
                                                }
                                                (KeyCode::Char('c'), m)
                                                    if m.contains(KeyModifiers::CONTROL) =>
                                                {
                                                    if crate::interrupt::handle_sigint() {
                                                        restore_terminal(&mut terminal, viewport_height);
                                                        tui_output::err_msg("Force quit.".into());
                                                        std::process::exit(130);
                                                    }
                                                    cancel_token.cancel();
                                                }
                                                (KeyCode::BackTab, _) => {
                                                    approval::cycle_mode(&shared_mode);
                                                }
                                                (KeyCode::Tab, KeyModifiers::NONE) => {
                                                    // Silent Tab completion during inference
                                                    // (no dropdown — would block the event loop)
                                                    let current = textarea.lines().join("\n");
                                                    if let Some(completed) = completer.complete(&current) {
                                                        textarea.select_all();
                                                        textarea.cut();
                                                        textarea.insert_str(&completed);
                                                    }
                                                }
                                                _ => {
                                                    completer.reset();
                                                    textarea.input(Event::Key(key));
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
                                                    renderer.preview_shown = true;
                                                }
                                                // Inline approval — uses crossterm direct writes
                                                let decision = crate::widgets::approval::prompt_approval(
                                                    &tool_name,
                                                    &detail,
                                                    preview.as_ref(),
                                                );
                                                // Resync ratatui viewport after crossterm writes
                                                crossterm_events = EventStream::new();
                                                terminal = init_terminal(viewport_height)?;
                                                let _ = cmd_tx
                                                    .send(EngineCommand::ApprovalResponse { id, decision })
                                                    .await;
                                            }
                                            UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
                                                // Show cap info via crossterm (matches approval widget path)
                                                tui_output::write_blank();
                                                tui_output::write_line(&Line::from(vec![
                                                    Span::raw("  "),
                                                    Span::styled(
                                                        format!("\u{26a0} Hard cap reached ({cap} iterations)"),
                                                        Style::default().fg(Color::Yellow),
                                                    ),
                                                ]));
                                                for name in &recent_tools {
                                                    tui_output::write_line(&Line::from(vec![
                                                        Span::raw("    "),
                                                        Span::styled(format!("\u{25cf} {name}"), Style::default().fg(Color::DarkGray)),
                                                    ]));
                                                }
                                                // Use approval widget for continue/stop
                                                let decision = crate::widgets::approval::prompt_approval(
                                                    "LoopCap",
                                                    "Continue running?",
                                                    None,
                                                );
                                                // Resync ratatui viewport after crossterm writes
                                                crossterm_events = EventStream::new();
                                                terminal = init_terminal(viewport_height)?;
                                                let action = match decision {
                                                    ApprovalDecision::Approve => koda_core::loop_guard::LoopContinuation::Continue200,
                                                    _ => koda_core::loop_guard::LoopContinuation::Stop,
                                                };
                                                let _ = cmd_tx
                                                    .send(EngineCommand::LoopDecision { action })
                                                    .await;
                                            }
                                            UiEvent::Engine(event) => {
                                                renderer.render_to_terminal(event, &mut terminal);
                                            }
                                        }
                                    }
                                }
                            }
                        } // end of pinned turn block

                        // Turn completed — cleanup
                        tui_state = TuiState::Idle;
                        inference_start = None;
                        crate::interrupt::reset();
                        session.cancel = tokio_util::sync::CancellationToken::new();

                        // Commit undo snapshots for this turn
                        if let Ok(mut undo) = agent.tools.undo.lock() {
                            undo.commit_turn();
                        }

                        // Drain remaining UI events
                        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
                            renderer.render_to_terminal(e, &mut terminal);
                        }

                        // Auto-compact
                        if config.auto_compact_threshold > 0 {
                            let ctx_pct = koda_core::context::percentage();
                            if ctx_pct >= config.auto_compact_threshold {
                                let pending = session
                                    .db
                                    .has_pending_tool_calls(&session.id)
                                    .await
                                    .unwrap_or(false);
                                if pending {
                                    if !silent_compact_deferred {
                                        emit_above(
                                            &mut terminal,
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
                                        silent_compact_deferred = true;
                                    }
                                } else {
                                    silent_compact_deferred = false;
                                    emit_above(
                                        &mut terminal,
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
                                        &session.db,
                                        &session.id,
                                        config.max_context_tokens,
                                        &config.model_settings,
                                        &provider,
                                    )
                                    .await
                                    {
                                        Ok(Ok(result)) => {
                                            emit_above(
                                                &mut terminal,
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
                                                &mut terminal,
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

        // Redraw viewport (resize if textarea grew/shrank)
        let mode = approval::read_mode(&shared_mode);
        let ctx = koda_core::context::percentage() as u32;
        let menu_extra = slash_menu.as_ref().map_or(0, |m| m.height());
        (terminal, viewport_height) =
            maybe_resize_viewport(terminal, &textarea, viewport_height, menu_extra)?;
        terminal.draw(|f| {
            draw_viewport(
                f,
                &textarea,
                &config.model,
                config.model_tier.label(),
                mode,
                ctx,
                tui_state,
                input_queue.len(),
                inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
                renderer.last_turn_stats.as_ref(),
                slash_menu.as_ref(),
            );
        })?;

        // ── Idle: wait for keyboard input ────────────────────

        tokio::select! {
            Some(Ok(ev)) = crossterm_events.next() => {
                if let Event::Resize(_, _) = ev {
                    // Terminal resized while idle — erase stale viewport and reinit.
                    terminal = reinit_viewport(terminal, viewport_height, viewport_height)?;
                } else if let Event::Key(key) = ev {
                    // ── Slash menu key interception ───────────
                    // When the menu is active, intercept navigation
                    // and selection keys before normal handling.
                    if slash_menu.is_some() {
                        match key.code {
                            KeyCode::Up => {
                                if let Some(ref mut menu) = slash_menu {
                                    menu.up();
                                }
                                continue;
                            }
                            KeyCode::Down | KeyCode::Tab => {
                                if let Some(ref mut menu) = slash_menu {
                                    menu.down();
                                }
                                continue;
                            }
                            KeyCode::Enter => {
                                if let Some(ref menu) = slash_menu {
                                    let cmd = menu.selected_command().to_string();
                                    textarea.select_all();
                                    textarea.cut();
                                    textarea.insert_str(&cmd);
                                }
                                slash_menu = None;
                                // Shrink viewport back to normal
                                terminal = reinit_viewport(terminal, viewport_height, MIN_VIEWPORT_HEIGHT)?;
                                viewport_height = MIN_VIEWPORT_HEIGHT;
                                continue;
                            }
                            KeyCode::Esc => {
                                slash_menu = None;
                                terminal = reinit_viewport(terminal, viewport_height, MIN_VIEWPORT_HEIGHT)?;
                                viewport_height = MIN_VIEWPORT_HEIGHT;
                                continue;
                            }
                            _ => {
                                // Fall through — let normal handlers process
                                // (e.g. Char, Backspace update textarea, then
                                //  the _ arm updates slash_menu state)
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
                            textarea.insert_newline();
                        }
                        (KeyCode::Enter, KeyModifiers::NONE) => {
                            // Paste detection: peek ahead for more input.
                            // If characters arrive within 30ms, it's a paste —
                            // insert newline instead of submitting.
                            let is_paste = tokio::time::timeout(
                                std::time::Duration::from_millis(30),
                                crossterm_events.next(),
                            )
                            .await;

                            match is_paste {
                                Ok(Some(Ok(Event::Key(next_key)))) => {
                                    // More input arrived quickly — it's a paste
                                    textarea.insert_newline();
                                    textarea.input(Event::Key(next_key));
                                }
                                _ => {
                                    // Timeout or no event — real Enter, submit
                                    let text = textarea.lines().join("\n");
                                    if !text.trim().is_empty() {
                                        textarea.select_all();
                                        textarea.cut();
                                        history.push(text.clone());
                                        save_history(&history);
                                        history_idx = None;
                                        let mode = approval::read_mode(&shared_mode);
                                        let icon = match mode {
                                            ApprovalMode::Safe => "🔍",
                                            ApprovalMode::Strict => "🔒",
                                            ApprovalMode::Auto => "⚡",
                                        };
                                        emit_above(&mut terminal, Line::from(vec![
                                            Span::styled(format!("{icon}> "), Style::default().fg(Color::Cyan)),
                                            Span::raw(text.clone()),
                                        ]));
                                        pending_command = Some(text);
                                    }
                                }
                            }
                        }
                        (KeyCode::Up, KeyModifiers::NONE)
                        | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                            if !history.is_empty() {
                                let idx = match history_idx {
                                    None => history.len() - 1,
                                    Some(i) => i.saturating_sub(1),
                                };
                                history_idx = Some(idx);
                                textarea.select_all();
                                textarea.cut();
                                textarea.insert_str(&history[idx]);
                            }
                        }
                        (KeyCode::Down, KeyModifiers::NONE)
                        | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                            if let Some(idx) = history_idx {
                                if idx + 1 < history.len() {
                                    history_idx = Some(idx + 1);
                                    textarea.select_all();
                                    textarea.cut();
                                    textarea.insert_str(&history[idx + 1]);
                                } else {
                                    history_idx = None;
                                    textarea.select_all();
                                    textarea.cut();
                                }
                            }
                        }
                        (KeyCode::Esc, _) => {
                            textarea.select_all();
                            textarea.cut();
                            history_idx = None;
                        }
                        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                            textarea.select_all();
                            textarea.cut();
                            history_idx = None;
                        }
                        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                            if textarea.lines().join("").trim().is_empty() {
                                should_quit = true;
                            }
                        }
                        (KeyCode::BackTab, _) => {
                            approval::cycle_mode(&shared_mode);
                            // Status bar updates on next draw — no scrollback noise
                        }
                        (KeyCode::Tab, KeyModifiers::NONE) => {
                            let current = textarea.lines().join("\n");
                            if let Some(completed) = completer.complete(&current) {
                                let candidates = completer.candidates();
                                if candidates.len() > 1 {
                                    // Multiple matches — open dropdown select menu
                                    let options: Vec<crate::select_menu::SelectOption> =
                                        candidates
                                            .iter()
                                            .map(|c| {
                                                crate::select_menu::SelectOption::new(c, "")
                                            })
                                            .collect();
                                    let initial = completer.selected_idx();
                                    if let Ok(Some(idx)) =
                                        crate::select_menu::select_inline(
                                            &mut terminal,
                                            "\u{1f4c2} Select",
                                            &options,
                                            initial,
                                        )
                                    {
                                        // Reconstruct the full text with the selected match
                                        let selected = &candidates[idx];
                                        // Rerun completion logic to build the full text
                                        let trimmed = current.trim_end();
                                        let replacement = if trimmed.starts_with('/') {
                                            selected.clone()
                                        } else if let Some(at_pos) =
                                            crate::completer::find_last_at_token(trimmed)
                                        {
                                            let prefix = &trimmed[..at_pos];
                                            format!("{prefix}@{selected}")
                                        } else {
                                            selected.clone()
                                        };
                                        textarea.select_all();
                                        textarea.cut();
                                        textarea.insert_str(&replacement);
                                    }
                                    // Reinit terminal after select_inline.
                                    // Drop EventStream first (same race fix as slash commands).
                                    crossterm_events = EventStream::new();
                                    terminal = init_terminal(viewport_height)?;
                                } else {
                                    // Single match — just insert it
                                    textarea.select_all();
                                    textarea.cut();
                                    textarea.insert_str(&completed);
                                }
                                completer.reset();
                            }
                        }
                        _ => {
                            history_idx = None;
                            completer.reset();
                            textarea.input(Event::Key(key));

                            // Update slash menu state reactively
                            let after_input = textarea.lines().join("\n");
                            let trimmed_after = after_input.trim_end();
                            if trimmed_after.starts_with('/') && !trimmed_after.contains(' ') {
                                slash_menu = crate::widgets::slash_menu::SlashMenuState::from_input(
                                    crate::completer::SLASH_COMMANDS,
                                    trimmed_after,
                                );
                                // Viewport resize handled by maybe_resize_viewport
                                // at the top of the loop
                            } else {
                                slash_menu = None;
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────

    restore_terminal(&mut terminal, viewport_height);
    {
        let mut mcp = agent.mcp_registry.write().await;
        mcp.shutdown();
    }

    crate::startup::print_resume_hint(&session.id);

    Ok(())
}

// ── History persistence ───────────────────────────────────────

const MAX_HISTORY: usize = 500;

fn history_file_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(config_dir).join("koda").join("history")
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

fn save_history(history: &[String]) {
    let path = history_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Keep only the last MAX_HISTORY entries
    let start = history.len().saturating_sub(MAX_HISTORY);
    let content = history[start..].join("\n");
    let _ = std::fs::write(&path, content);
}
