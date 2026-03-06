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
use crate::repl::{self, ReplAction};
use crate::sink::UiEvent;
use crate::tui::{self, SelectOption};
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

fn draw_viewport(
    frame: &mut ratatui::Frame,
    textarea: &TextArea,
    model: &str,
    mode: ApprovalMode,
    context_pct: u32,
    state: TuiState,
    queue_len: usize,
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
    frame.render_widget(&*textarea, text_area);

    // Status bar
    let mut sb = StatusBar::new(model, mode.label(), context_pct);
    if queue_len > 0 {
        sb = sb.with_queue(queue_len);
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
    drop(std::mem::replace(
        terminal,
        // Dummy — we just need to call clear() on the original
        Terminal::with_options(
            CrosstermBackend::new(std::io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(0),
            },
        )
        .unwrap(),
    ));
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

                        let action = handle_slash_command(
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
                        // Start inference turn
                        start_inference_turn(
                            &input,
                            &mut terminal,
                            &mut session,
                            &config,
                            &shared_mode,
                            &project_root,
                            &ui_tx,
                        )
                        .await;
                        tui_state = TuiState::Inferring;
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
            );
        })?;

        // ── Unified select loop ──────────────────────────────

        tokio::select! {
            // Keyboard events (always active)
            Some(Ok(ev)) = crossterm_events.next() => {
                if let Event::Key(key) = ev {
                    match (key.code, key.modifiers, tui_state) {
                        // Enter → submit
                        (KeyCode::Enter, KeyModifiers::NONE, _) => {
                            let text = textarea.lines().join("\n");
                            if !text.trim().is_empty() {
                                textarea.select_all();
                                textarea.cut();

                                if tui_state == TuiState::Idle {
                                    // Echo and process immediately
                                    let mode = approval::read_mode(&shared_mode);
                                    let prompt = repl::format_prompt(&config.model, mode);
                                    emit_above(&mut terminal, Line::raw(format!("{prompt}{text}")));
                                    pending_command = Some(text);
                                } else {
                                    // Queue for later
                                    input_queue.push_back(text);
                                }
                            }
                        }
                        // Esc → cancel inference if running, else clear input
                        (KeyCode::Esc, _, TuiState::Inferring) => {
                            session.cancel.cancel();
                        }
                        (KeyCode::Esc, _, TuiState::Idle) => {
                            textarea.select_all();
                            textarea.cut();
                        }
                        // Ctrl+C → cancel inference or clear input
                        (KeyCode::Char('c'), m, _) if m.contains(KeyModifiers::CONTROL) => {
                            if tui_state == TuiState::Inferring {
                                if crate::interrupt::handle_sigint() {
                                    restore_terminal(&mut terminal);
                                    eprintln!("\x1b[31mForce quit.\x1b[0m");
                                    std::process::exit(130);
                                }
                                session.cancel.cancel();
                            } else {
                                textarea.select_all();
                                textarea.cut();
                            }
                        }
                        // Ctrl+D on empty → EOF
                        (KeyCode::Char('d'), m, TuiState::Idle)
                            if m.contains(KeyModifiers::CONTROL) =>
                        {
                            if textarea.lines().join("").trim().is_empty() {
                                should_quit = true;
                            }
                        }
                        // Shift+Tab → cycle approval mode
                        (KeyCode::BackTab, _, _) => {
                            approval::cycle_mode(&shared_mode);
                        }
                        // All other keys → forward to textarea
                        _ => {
                            textarea.input(Event::Key(key));
                        }
                    }
                }
            }

            // Engine events (during inference)
            Some(ui_event) = ui_rx.recv(), if tui_state == TuiState::Inferring => {
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

                        // Re-enter raw mode
                        crossterm::terminal::enable_raw_mode()?;
                        terminal = init_terminal()?;
                        crossterm_events = EventStream::new();
                    }
                    UiEvent::Engine(EngineEvent::LoopCapReached { cap, recent_tools }) => {
                        // Exit raw mode for interactive prompt
                        let _ = terminal.clear();
                        let _ = crossterm::terminal::disable_raw_mode();
                        print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
                        let _ = std::io::Write::flush(&mut std::io::stdout());

                        let action = crate::app::cli_loop_continue_prompt(cap, &recent_tools);
                        let _ = cmd_tx
                            .send(EngineCommand::LoopDecision { action })
                            .await;

                        // Re-enter raw mode
                        crossterm::terminal::enable_raw_mode()?;
                        terminal = init_terminal()?;
                        crossterm_events = EventStream::new();
                    }
                    UiEvent::Engine(EngineEvent::TurnStart { .. }) => {
                        tui_state = TuiState::Inferring;
                    }
                    UiEvent::Engine(EngineEvent::TurnEnd { .. }) => {
                        tui_state = TuiState::Idle;
                        renderer.stop_spinner();
                        crate::interrupt::reset();
                        session.cancel = tokio_util::sync::CancellationToken::new();

                        // Auto-compact check
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
                                                    format!("\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)"),
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
                                                format!("\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting..."),
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
                    UiEvent::Engine(ref event) => {
                        renderer.render_to_terminal(event.clone(), &mut terminal);
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

// ── Slash command handler ────────────────────────────────────

enum SlashAction {
    Continue,
    Quit,
}

#[allow(clippy::too_many_arguments)]
async fn handle_slash_command(
    input: &str,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    session: &mut KodaSession,
    shared_mode: &approval::SharedMode,
    renderer: &mut TuiRenderer,
    project_root: &PathBuf,
    agent: &Arc<KodaAgent>,
    pending_command: &mut Option<String>,
) -> SlashAction {
    match repl::handle_command(input, config, provider).await {
        ReplAction::Quit => SlashAction::Quit,
        ReplAction::SwitchModel(model) => {
            config.model = model.clone();
            config.model_settings.model = model.clone();
            let mut s = koda_core::approval::Settings::load();
            let _ = s.save_last_provider(
                &config.provider_type.to_string(),
                &config.base_url,
                &config.model,
            );
            println!("  \x1b[32m\u{2713}\x1b[0m Model set to: \x1b[36m{model}\x1b[0m");
            SlashAction::Continue
        }
        ReplAction::PickModel => {
            let prov = provider.read().await;
            match prov.list_models().await {
                Ok(models) if models.is_empty() => {
                    println!(
                        "  \x1b[33mNo models available from {}\x1b[0m",
                        prov.provider_name()
                    );
                }
                Ok(models) => {
                    drop(prov);
                    let current_idx = models
                        .iter()
                        .position(|m| m.id == config.model)
                        .unwrap_or(0);
                    let options: Vec<SelectOption> = models
                        .iter()
                        .map(|m| {
                            let desc = if m.id == config.model {
                                "\u{25c0} current".to_string()
                            } else {
                                String::new()
                            };
                            SelectOption::new(&m.id, desc)
                        })
                        .collect();
                    match tui::select("\u{1f43b} Select a model", &options, current_idx) {
                        Ok(Some(idx)) => {
                            config.model = models[idx].id.clone();
                            config.model_settings.model = config.model.clone();
                            let mut s = koda_core::approval::Settings::load();
                            let _ = s.save_last_provider(
                                &config.provider_type.to_string(),
                                &config.base_url,
                                &config.model,
                            );
                            println!(
                                "  \x1b[32m\u{2713}\x1b[0m Model set to: \x1b[36m{}\x1b[0m",
                                config.model
                            );
                        }
                        Ok(None) => println!("  \x1b[90mCancelled.\x1b[0m"),
                        Err(e) => println!("  \x1b[31mTUI error: {e}\x1b[0m"),
                    }
                }
                Err(e) => println!("  \x1b[31mFailed to list models: {e}\x1b[0m"),
            }
            SlashAction::Continue
        }
        ReplAction::SetupProvider(ptype, base_url) => {
            crate::commands::handle_setup_provider(config, provider, ptype, base_url).await;
            SlashAction::Continue
        }
        ReplAction::PickProvider => {
            crate::commands::handle_pick_provider(config, provider).await;
            SlashAction::Continue
        }
        ReplAction::ShowHelp => {
            let commands = [
                ("/agent", "List available sub-agents"),
                ("/compact", "Summarize conversation to reclaim context"),
                ("/cost", "Show token usage for this session"),
                ("/diff", "Show git diff / review / commit message"),
                ("/expand", "Show full output of last tool call (/expand N)"),
                ("/mcp", "MCP servers: status / add / remove / restart"),
                ("/memory", "View/save project & global memory"),
                ("/model", "Pick a model interactively"),
                ("/provider", "Switch LLM provider"),
                ("/sessions", "List/resume/delete sessions"),
                ("/trust", "Set approval mode (always / auto / never)"),
                ("/verbose", "Toggle full tool output (on/off)"),
                ("/exit", "Quit the session"),
            ];
            let options: Vec<SelectOption> = commands
                .iter()
                .map(|(cmd, desc)| SelectOption::new(*cmd, *desc))
                .collect();
            if let Ok(Some(idx)) = tui::select("\u{1f43b} Commands", &options, 0) {
                let (cmd, _) = commands[idx];
                *pending_command = Some(cmd.to_string());
            }
            println!();
            println!(
                "  \x1b[90mTips: @file to attach context \u{00b7} Shift+Tab to cycle mode \u{00b7} Ctrl+C to cancel \u{00b7} Ctrl+D to exit\x1b[0m"
            );
            SlashAction::Continue
        }
        ReplAction::ShowCost => {
            match session.db.session_token_usage(&session.id).await {
                Ok(u) => {
                    let total = u.prompt_tokens
                        + u.completion_tokens
                        + u.cache_read_tokens
                        + u.cache_creation_tokens;
                    println!();
                    println!("  \x1b[1m\u{1f43b} Session Cost\x1b[0m");
                    println!();
                    println!("  Prompt tokens:     \x1b[36m{:>8}\x1b[0m", u.prompt_tokens);
                    println!(
                        "  Completion tokens: \x1b[36m{:>8}\x1b[0m",
                        u.completion_tokens
                    );
                    if u.cache_read_tokens > 0 {
                        println!(
                            "  Cache read tokens: \x1b[32m{:>8}\x1b[0m",
                            u.cache_read_tokens
                        );
                    }
                    if u.cache_creation_tokens > 0 {
                        println!(
                            "  Cache write tokens:\x1b[33m{:>8}\x1b[0m",
                            u.cache_creation_tokens
                        );
                    }
                    if u.thinking_tokens > 0 {
                        println!(
                            "  Thinking tokens:   \x1b[35m{:>8}\x1b[0m",
                            u.thinking_tokens
                        );
                    }
                    println!("  Total tokens:      \x1b[1m{total:>8}\x1b[0m");
                    println!("  API calls:         \x1b[90m{:>8}\x1b[0m", u.api_calls);
                    println!();
                    println!("  \x1b[90mModel: {}\x1b[0m", config.model);
                    println!("  \x1b[90mProvider: {}\x1b[0m", config.provider_type);
                }
                Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
            }
            SlashAction::Continue
        }
        ReplAction::ListSessions => {
            match session.db.list_sessions(10, project_root).await {
                Ok(sessions) if sessions.is_empty() => {
                    println!("  \x1b[90mNo other sessions found.\x1b[0m");
                }
                Ok(sessions) => {
                    let current_idx = sessions
                        .iter()
                        .position(|s| s.id == session.id)
                        .unwrap_or(0);
                    let options: Vec<SelectOption> = sessions
                        .iter()
                        .map(|s| {
                            let desc = if s.id == session.id {
                                format!(
                                    "{}  {} msgs  {}k tokens  \u{25c0} current",
                                    s.created_at,
                                    s.message_count,
                                    s.total_tokens / 1000
                                )
                            } else {
                                format!(
                                    "{}  {} msgs  {}k tokens",
                                    s.created_at,
                                    s.message_count,
                                    s.total_tokens / 1000
                                )
                            };
                            SelectOption::new(&s.id[..8], desc)
                        })
                        .collect();
                    match tui::select("\u{1f43b} Sessions", &options, current_idx) {
                        Ok(Some(idx)) => {
                            let target = &sessions[idx];
                            if target.id == session.id {
                                println!("  \x1b[90mAlready in this session.\x1b[0m");
                            } else {
                                session.id = target.id.clone();
                                println!(
                                    "  \x1b[32m\u{2713}\x1b[0m Resumed session \x1b[36m{}\x1b[0m  \x1b[90m{}  {} msgs\x1b[0m",
                                    &target.id[..8],
                                    target.created_at,
                                    target.message_count,
                                );
                            }
                        }
                        Ok(None) => println!("  \x1b[90mCancelled.\x1b[0m"),
                        Err(e) => println!("  \x1b[31mTUI error: {e}\x1b[0m"),
                    }
                    println!("  \x1b[90mDelete: /sessions delete <id>\x1b[0m");
                }
                Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
            }
            SlashAction::Continue
        }
        ReplAction::DeleteSession(ref id) => {
            if id == &session.id {
                println!("  \x1b[31mCannot delete the current session.\x1b[0m");
            } else {
                match session.db.list_sessions(100, project_root).await {
                    Ok(sessions) => {
                        let matches: Vec<_> =
                            sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                        match matches.len() {
                            0 => println!("  \x1b[31mNo session found matching '{id}'.\x1b[0m"),
                            1 => {
                                let full_id = &matches[0].id;
                                match session.db.delete_session(full_id).await {
                                    Ok(true) => println!(
                                        "  \x1b[32m\u{2713}\x1b[0m Deleted session {}",
                                        &full_id[..8]
                                    ),
                                    Ok(false) => {
                                        println!("  \x1b[31mSession not found.\x1b[0m")
                                    }
                                    Err(e) => {
                                        println!("  \x1b[31mError: {e}\x1b[0m")
                                    }
                                }
                            }
                            n => println!(
                                "  \x1b[31mAmbiguous: '{id}' matches {n} sessions. Be more specific.\x1b[0m"
                            ),
                        }
                    }
                    Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                }
            }
            SlashAction::Continue
        }
        ReplAction::ResumeSession(ref id) => {
            if session.id.starts_with(id) {
                println!("  \x1b[90mAlready in this session.\x1b[0m");
            } else {
                match session.db.list_sessions(100, project_root).await {
                    Ok(sessions) => {
                        let matches: Vec<_> =
                            sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                        match matches.len() {
                            0 => println!("  \x1b[31mNo session found matching '{id}'.\x1b[0m"),
                            1 => {
                                let target = &matches[0];
                                session.id = target.id.clone();
                                println!(
                                    "  \x1b[32m\u{2713}\x1b[0m Resumed session \x1b[36m{}\x1b[0m  \x1b[90m{}  {} msgs\x1b[0m",
                                    &target.id[..8],
                                    target.created_at,
                                    target.message_count,
                                );
                            }
                            n => println!(
                                "  \x1b[31mAmbiguous: '{id}' matches {n} sessions. Be more specific.\x1b[0m"
                            ),
                        }
                    }
                    Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
                }
            }
            SlashAction::Continue
        }
        ReplAction::InjectPrompt(prompt) => {
            *pending_command = Some(prompt);
            SlashAction::Continue
        }
        ReplAction::Compact => {
            crate::commands::handle_compact(&session.db, &session.id, config, provider, false)
                .await;
            SlashAction::Continue
        }
        ReplAction::McpCommand(ref args) => {
            crate::commands::handle_mcp_command(args, &agent.mcp_registry, project_root).await;
            SlashAction::Continue
        }
        ReplAction::SetTrust(mode_name) => {
            let new_mode = if let Some(ref name) = mode_name {
                ApprovalMode::parse(name)
            } else {
                crate::commands::pick_trust_mode(approval::read_mode(shared_mode))
            };
            if let Some(m) = new_mode {
                approval::set_mode(shared_mode, m);
                println!(
                    "  \x1b[32m\u{2713}\x1b[0m Trust: \x1b[1m{}\x1b[0m \u{2014} {}",
                    m.label(),
                    m.description()
                );
            } else if let Some(ref name) = mode_name {
                println!(
                    "  \x1b[31m\u{2717}\x1b[0m Unknown trust level '{}'. Use: plan, normal, yolo",
                    name
                );
            }
            SlashAction::Continue
        }
        ReplAction::Expand(n) => {
            match renderer.tool_history.get(n) {
                Some(record) => {
                    crate::display::print_tool_output_full(record);
                }
                None => {
                    let total = renderer.tool_history.len();
                    if total == 0 {
                        println!("  \x1b[90mNo tool outputs recorded yet.\x1b[0m");
                    } else {
                        println!(
                            "  \x1b[33mNo tool output #{n}. Have {total} recorded (use /expand 1\u{2013}{total}).\x1b[0m"
                        );
                    }
                }
            }
            SlashAction::Continue
        }
        ReplAction::Verbose(v) => {
            renderer.verbose = match v {
                Some(val) => val,
                None => !renderer.verbose,
            };
            let state = if renderer.verbose { "on" } else { "off" };
            println!("  \x1b[36mVerbose tool output: {state}\x1b[0m");
            SlashAction::Continue
        }
        ReplAction::Handled => SlashAction::Continue,
        ReplAction::NotACommand => SlashAction::Continue,
    }
}

// ── Inference turn kickoff ───────────────────────────────────

async fn start_inference_turn(
    input: &str,
    terminal: &mut Term,
    session: &mut KodaSession,
    config: &KodaConfig,
    shared_mode: &approval::SharedMode,
    project_root: &PathBuf,
    ui_tx: &mpsc::Sender<UiEvent>,
) {
    let processed = input::process_input(input, project_root);
    if !processed.images.is_empty() {
        for (i, _img) in processed.images.iter().enumerate() {
            emit_above(
                terminal,
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

    let user_message = if let Some(context) = input::format_context_files(&processed.context_files)
    {
        if !processed.context_files.is_empty() {
            for f in &processed.context_files {
                emit_above(
                    terminal,
                    Line::from(vec![
                        ratatui::text::Span::raw("  "),
                        ratatui::text::Span::styled(
                            format!("\u{1f4ce} {}", f.path),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]),
                );
            }
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

    session.mode = approval::read_mode(shared_mode);
    session.update_provider(config);

    let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());

    // Spawn inference as a background task
    let config = config.clone();
    // We can't move session into the task, so we run_turn on the current task
    // via the event loop — the turn future is driven by select! receiving ui events.
    // Actually, we need to spawn this differently. Let's use a JoinHandle approach.
    let cancel = session.cancel.clone();
    let mut cmd_rx_placeholder = mpsc::channel::<EngineCommand>(32).1;

    // For now, spawn the turn in a task. The cmd_rx is shared via the channel.
    // TODO: This needs proper wiring — the cmd_rx should be shared.
    tokio::spawn({
        // We can't move session into a task. We need a different approach.
        // For now, let's keep the run_turn inline in the event loop.
        async {}
    });
}
