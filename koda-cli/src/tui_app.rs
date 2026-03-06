//! TUI-based interactive event loop with persistent inline viewport.
//!
//! Architecture:
//!   - Terminal stays in raw mode for the entire session
//!   - `Viewport::Inline(2)` with `scrolling-regions` is always rendered
//!   - Output scrolls above the viewport via `terminal.insert_before()`
//!   - Input is always active — type-ahead queues submissions during inference
//!
//! ```text
//! Normal terminal scrollback            ← via insert_before()
//!   ├── Tool banners, markdown, diffs
//!   └── All EngineEvent rendering
//!
//! ┌─ ratatui Viewport::Inline(2) ──────────────────────────┐
//! │ 🐻> user types here even during inference_              │
//! │ model │ normal │ ████░░ 5%                             │
//! └────────────────────────────────────────────────────────┘
//! ```

use crate::input;
use crate::repl;
use crate::sink::UiEvent;
use crate::tui_commands::{self, SlashAction};
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
    text::Line,
    widgets::Paragraph,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

/// Height of the inline viewport (input line + status bar).
const VIEWPORT_HEIGHT: u16 = 2;

// ── Session state ────────────────────────────────────────────

/// What the TUI is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiState {
    /// Waiting for user input (no inference running).
    Idle,
    /// An inference turn is running.
    Inferring,
}

// ── Approval helper ──────────────────────────────────────────

fn map_confirmation(c: crate::confirm::Confirmation) -> ApprovalDecision {
    use crate::confirm::Confirmation;
    match c {
        Confirmation::Approved => ApprovalDecision::Approve,
        Confirmation::Rejected => ApprovalDecision::Reject,
        Confirmation::RejectedWithFeedback(fb) => {
            ApprovalDecision::RejectWithFeedback { feedback: fb }
        }
        Confirmation::AlwaysAllow => ApprovalDecision::AlwaysAllow,
    }
}

// ── Viewport drawing ─────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_viewport(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    model: &str,
    mode: ApprovalMode,
    context_pct: u32,
    state: TuiState,
    queue_len: usize,
    elapsed_secs: u64,
) {
    let area = frame.area();
    let [input_row, status_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Prompt icon + textarea
    let (icon, color) = match (state, mode) {
        (TuiState::Inferring, _) => ("\u{23f3}", Color::DarkGray), // ⏳ during inference
        (_, ApprovalMode::Plan) => ("\u{1f4cb}", Color::Yellow),
        (_, ApprovalMode::Normal) => ("\u{1f43b}", Color::Cyan),
        (_, ApprovalMode::Yolo) => ("\u{26a1}", Color::Red),
    };
    let prompt_width: u16 = 4;
    let [prompt_area, text_area] =
        Layout::horizontal([Constraint::Length(prompt_width), Constraint::Fill(1)])
            .areas(input_row);

    frame.render_widget(
        Paragraph::new(format!("{icon}> ")).style(Style::default().fg(color)),
        prompt_area,
    );
    frame.render_widget(textarea, text_area);

    // Status bar
    let mut sb = StatusBar::new(model, mode.label(), context_pct);
    if queue_len > 0 {
        sb = sb.with_queue(queue_len);
    }
    if elapsed_secs > 0 {
        sb = sb.with_elapsed(elapsed_secs);
    }
    frame.render_widget(sb, status_row);
}

// ── Terminal lifecycle helpers ────────────────────────────────

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn init_terminal() -> Result<Term> {
    crossterm::terminal::enable_raw_mode()?;
    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Term) {
    let _ = terminal.clear();
    let _ = crossterm::terminal::disable_raw_mode();
    // Erase leftover viewport lines
    print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
    let _ = std::io::Write::flush(&mut std::io::stdout());
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
    }

    let provider: Arc<RwLock<Box<dyn LlmProvider>>> =
        Arc::new(RwLock::new(crate::commands::create_provider(&config)));

    if config.model == "auto-detect" {
        let prov = provider.read().await;
        match prov.list_models().await {
            Ok(models) if !models.is_empty() => {
                config.model = models[0].id.clone();
                config.model_settings.model = config.model.clone();
                tracing::info!("Auto-detected model: {}", config.model);
            }
            Ok(_) => {
                config.model = "(no model loaded)".to_string();
                config.model_settings.model = config.model.clone();
                // Print to stderr (before raw mode)
                eprintln!(
                    "  \x1b[33m\u{26a0} No model loaded in {}.\x1b[0m",
                    config.provider_type
                );
                eprintln!("    Load a model, then use \x1b[36m/model\x1b[0m to select it.");
            }
            Err(e) => {
                config.model = "(connection failed)".to_string();
                config.model_settings.model = config.model.clone();
                eprintln!(
                    "  \x1b[31m\u{2717} Could not connect to {} at {}\x1b[0m",
                    config.provider_type, config.base_url
                );
                tracing::warn!("Auto-detect failed: {e}");
            }
        }
    }

    // Print banner BEFORE entering raw mode
    let recent = db.recent_user_messages(3).await.unwrap_or_default();
    repl::print_banner(&config, &session_id, &recent);

    if let Ok(Some(latest)) = version_check.await
        && let Some((current, latest)) = koda_core::version::update_available(&latest)
    {
        let crate_name = koda_core::version::crate_name();
        println!(
            "  \x1b[90m\u{2728} Update available: \x1b[0m\x1b[36m{current}\x1b[0m\x1b[90m \u{2192} \x1b[0m\x1b[32m{latest}\x1b[0m\x1b[90m  (cargo install {crate_name})\x1b[0m"
        );
        println!();
    }

    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);

    if !agent.mcp_statuses.is_empty() {
        println!(
            "  \x1b[36m\u{1f50c} Connecting to {} MCP server(s)...\x1b[0m",
            agent.mcp_statuses.len()
        );
        for (name, result) in &agent.mcp_statuses {
            match result {
                Ok(tool_count) => {
                    println!("  \x1b[32m\u{2713}\x1b[0m {name} — {tool_count} tool(s)");
                }
                Err(msg) => {
                    println!("  \x1b[31m\u{2717}\x1b[0m {name} — {msg}");
                }
            }
        }
        println!();
    }

    let mut session = KodaSession::new(
        session_id.clone(),
        agent.clone(),
        db,
        &config,
        ApprovalMode::Normal,
    );

    let shared_mode = approval::new_shared_mode(ApprovalMode::Normal);

    // ── Initialize persistent terminal ───────────────────────

    let mut terminal = init_terminal()?;

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
    let mut tui_state = TuiState::Idle;
    let mut input_queue: VecDeque<String> = VecDeque::new();
    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;
    let mut should_quit = false;
    let mut inference_start: Option<std::time::Instant> = None;

    // Crossterm event stream for async key capture
    let mut crossterm_events = EventStream::new();

    // ── Initial viewport draw ────────────────────────────────

    let mode = approval::read_mode(&shared_mode);
    let ctx = koda_core::context::percentage() as u32;
    terminal.draw(|f| {
        draw_viewport(
            f,
            &textarea,
            &config.model,
            mode,
            ctx,
            tui_state,
            input_queue.len(),
            inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
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
                let prompt = repl::format_prompt(&config.model, mode);
                emit_above(&mut terminal, Line::raw(format!("{prompt}{queued}")));
                Some(queued)
            } else {
                None
            };

            if let Some(input) = input {
                let input = input.trim().to_string();
                if !input.is_empty() {
                    // Try slash commands first
                    if input.starts_with('/') {
                        // Temporarily exit raw mode for TUI-based slash commands
                        let _ = terminal.clear();
                        let _ = crossterm::terminal::disable_raw_mode();
                        print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
                        let _ = std::io::Write::flush(&mut std::io::stdout());

                        let action = tui_commands::handle_slash_command(
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
                            SlashAction::Continue => {}
                            SlashAction::Quit => {
                                println!("\x1b[36m\u{1f43b} Goodbye!\x1b[0m");
                                should_quit = true;
                                continue;
                            }
                        }

                        // Re-enter raw mode
                        crossterm::terminal::enable_raw_mode()?;
                        terminal = init_terminal()?;
                        crossterm_events = EventStream::new();
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
                                        mode,
                                        ctx,
                                        tui_state,
                                        input_queue.len(),
                                        inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
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
                                                        format!("\u{2717} Turn failed: {e}"),
                                                        Style::default().fg(Color::Red),
                                                    ),
                                                ]),
                                            );
                                        }
                                        break;
                                    }
                                    Some(Ok(ev)) = crossterm_events.next() => {
                                        if let Event::Key(key) = ev {
                                            match (key.code, key.modifiers) {
                                                (KeyCode::Enter, KeyModifiers::NONE) => {
                                                    let text = textarea.lines().join("\n");
                                                    if !text.trim().is_empty() {
                                                        textarea.select_all();
                                                        textarea.cut();
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
                                                        restore_terminal(&mut terminal);
                                                        eprintln!("\x1b[31mForce quit.\x1b[0m");
                                                        std::process::exit(130);
                                                    }
                                                    cancel_token.cancel();
                                                }
                                                (KeyCode::BackTab, _) => {
                                                    approval::cycle_mode(&shared_mode);
                                                }
                                                _ => {
                                                    textarea.input(Event::Key(key));
                                                }
                                            }
                                        }
                                    }
                                    Some(ui_event) = ui_rx.recv() => {
                                        match ui_event {
                                            UiEvent::Engine(EngineEvent::ApprovalRequest {
                                                id, tool_name, detail, preview, whitelist_hint,
                                            }) => {
                                                if preview.is_some() {
                                                    renderer.preview_shown = true;
                                                }
                                                // Exit raw mode for approval UI
                                                let _ = terminal.clear();
                                                let _ = crossterm::terminal::disable_raw_mode();
                                                print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
                                                let _ = std::io::Write::flush(&mut std::io::stdout());

                                                let decision = map_confirmation(
                                                    crate::confirm::confirm_tool_action(
                                                        &tool_name,
                                                        &detail,
                                                        preview.as_ref(),
                                                        whitelist_hint.as_deref(),
                                                    ),
                                                );
                                                let _ = cmd_tx
                                                    .send(EngineCommand::ApprovalResponse { id, decision })
                                                    .await;

                                                crossterm::terminal::enable_raw_mode()?;
                                                terminal = init_terminal()?;
                                                crossterm_events = EventStream::new();
                                            }
                                            UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
                                                let _ = terminal.clear();
                                                let _ = crossterm::terminal::disable_raw_mode();
                                                print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
                                                let _ = std::io::Write::flush(&mut std::io::stdout());

                                                let action = crate::app::cli_loop_continue_prompt(cap, &recent_tools);
                                                let _ = cmd_tx
                                                    .send(EngineCommand::LoopDecision { action })
                                                    .await;

                                                crossterm::terminal::enable_raw_mode()?;
                                                terminal = init_terminal()?;
                                                crossterm_events = EventStream::new();
                                            }
                                            UiEvent::Engine(ref event) => {
                                                renderer.render_to_terminal(event.clone(), &mut terminal);
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
                                    crate::commands::handle_compact(
                                        &session.db,
                                        &session.id,
                                        &config,
                                        &provider,
                                        true,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Redraw viewport
        let mode = approval::read_mode(&shared_mode);
        let ctx = koda_core::context::percentage() as u32;
        terminal.draw(|f| {
            draw_viewport(
                f,
                &textarea,
                &config.model,
                mode,
                ctx,
                tui_state,
                input_queue.len(),
                inference_start.map(|s| s.elapsed().as_secs()).unwrap_or(0),
            );
        })?;

        // ── Idle: wait for keyboard input ────────────────────

        tokio::select! {
            Some(Ok(ev)) = crossterm_events.next() => {
                if let Event::Key(key) = ev {
                    match (key.code, key.modifiers) {
                        (KeyCode::Enter, KeyModifiers::NONE) => {
                            let text = textarea.lines().join("\n");
                            if !text.trim().is_empty() {
                                textarea.select_all();
                                textarea.cut();
                                let mode = approval::read_mode(&shared_mode);
                                let prompt = repl::format_prompt(&config.model, mode);
                                emit_above(&mut terminal, Line::raw(format!("{prompt}{text}")));
                                pending_command = Some(text);
                            }
                        }
                        (KeyCode::Esc, _) => {
                            textarea.select_all();
                            textarea.cut();
                        }
                        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                            textarea.select_all();
                            textarea.cut();
                        }
                        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                            if textarea.lines().join("").trim().is_empty() {
                                should_quit = true;
                            }
                        }
                        (KeyCode::BackTab, _) => {
                            approval::cycle_mode(&shared_mode);
                        }
                        _ => {
                            textarea.input(Event::Key(key));
                        }
                    }
                }
            }
        }
    }

    // ── Cleanup ───────────────────────────────────────────────

    restore_terminal(&mut terminal);

    {
        let mut mcp = agent.mcp_registry.write().await;
        mcp.shutdown();
    }

    println!(
        "\n\x1b[90mResume this session with:\n  koda --resume {}\x1b[0m",
        session.id
    );

    Ok(())
}
