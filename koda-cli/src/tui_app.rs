//! TUI-based interactive event loop.
//!
//! Replaces the rustyline-based `app.rs` with `ratatui` + `tui-textarea`
//! for inline input and status bar. All output (`display.rs`, `markdown.rs`)
//! remains as `println!` to preserve native terminal scrollback.
//!
//! Architecture:
//!   Input phase  -> ratatui Viewport::Inline(2) with tui-textarea + status bar
//!   Output phase -> viewport cleared, normal println! (unchanged)

use crate::input;
use crate::repl::{self, ReplAction};
use crate::sink::{UiEvent, UiRenderer};
use crate::tui::{self, SelectOption};
use crate::widgets::status_bar::StatusBar;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
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
    widgets::Paragraph,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tui_textarea::TextArea;

/// Height of the inline viewport (input line + status bar).
const VIEWPORT_HEIGHT: u16 = 2;

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

// ── Inline TUI input ─────────────────────────────────────────

/// Result from the TUI input loop.
enum InputResult {
    /// User submitted text (may be empty).
    Line(String),
    /// User pressed Ctrl+D on empty buffer (EOF).
    Eof,
}

/// Read user input via ratatui inline viewport with tui-textarea.
///
/// This function is blocking — it should be called via `spawn_blocking`.
/// It creates a temporary ratatui terminal, handles key events, and cleans
/// up before returning.
fn read_input(model: &str, shared_mode: &approval::SharedMode) -> Result<InputResult> {
    crossterm::terminal::enable_raw_mode()?;

    let stdout = std::io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;

    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::REVERSED),
    );
    textarea.set_placeholder_text("Type a message...");
    textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));

    let result = input_loop(&mut terminal, &mut textarea, shared_mode, model);

    // Clean up: clear viewport, restore terminal
    let _ = terminal.clear();
    drop(terminal);
    crossterm::terminal::disable_raw_mode()?;

    // Erase the viewport lines left on screen
    print!("\x1b[{}A\x1b[J", VIEWPORT_HEIGHT);
    let _ = std::io::Write::flush(&mut std::io::stdout());

    result
}

/// Inner event loop for tui-textarea input.
fn input_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    textarea: &mut TextArea,
    shared_mode: &approval::SharedMode,
    model: &str,
) -> Result<InputResult> {
    loop {
        let mode = approval::read_mode(shared_mode);
        let context_pct = koda_core::context::percentage() as u32;

        terminal.draw(|frame| {
            let area = frame.area();
            let [input_row, status_row] =
                Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

            // Prompt icon + textarea
            let (icon, color) = match mode {
                ApprovalMode::Plan => ("\u{1f4cb}", Color::Yellow),
                ApprovalMode::Normal => ("\u{1f43b}", Color::Cyan),
                ApprovalMode::Yolo => ("\u{26a1}", Color::Red),
            };
            let prompt_width: u16 = 4; // emoji(2) + >(1) + space(1)
            let [prompt_area, text_area] =
                Layout::horizontal([Constraint::Length(prompt_width), Constraint::Fill(1)])
                    .areas(input_row);

            frame.render_widget(
                Paragraph::new(format!("{icon}> ")).style(Style::default().fg(color)),
                prompt_area,
            );
            frame.render_widget(&*textarea, text_area);

            // Status bar
            frame.render_widget(StatusBar::new(model, mode.label(), context_pct), status_row);
        })?;

        // Block until next terminal event
        let ev = event::read()?;

        let handled = if let Event::Key(key) = &ev {
            match (key.code, key.modifiers) {
                // Enter → submit
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    let text = textarea.lines().join("\n");
                    return Ok(InputResult::Line(text));
                }
                // Esc → clear input
                (KeyCode::Esc, _) => {
                    textarea.select_all();
                    textarea.cut();
                    true
                }
                // Ctrl+C → clear input
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    textarea.select_all();
                    textarea.cut();
                    true
                }
                // Ctrl+D on empty → EOF
                (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                    if textarea.lines().join("").trim().is_empty() {
                        return Ok(InputResult::Eof);
                    }
                    false // let textarea handle delete-forward
                }
                // Shift+Tab → cycle approval mode
                (KeyCode::BackTab, _) => {
                    approval::cycle_mode(shared_mode);
                    true
                }
                _ => false,
            }
        } else {
            false
        };

        if !handled {
            textarea.input(ev);
        }
    }
}

// ── Main event loop ──────────────────────────────────────────

/// Run the main interactive event loop with TUI input.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
) -> Result<()> {
    // ── Setup (same as app.rs) ───────────────────────────────

    // Restore last-used provider/model if available
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

    // Auto-detect the serving model for local providers
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

    let recent = db.recent_user_messages(3).await.unwrap_or_default();
    repl::print_banner(&config, &session_id, &recent);

    // Show update hint if version check completed
    if let Ok(Some(latest)) = version_check.await {
        koda_core::version::print_update_hint(&latest);
    }

    // Build agent (tools, MCP, system prompt) and session
    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);
    let mut session = KodaSession::new(
        session_id.clone(),
        agent.clone(),
        db,
        &config,
        ApprovalMode::Normal,
    );

    let shared_mode = approval::new_shared_mode(ApprovalMode::Normal);

    // ── Channels ─────────────────────────────────────────────

    // Engine sink -> main loop (UI events)
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);

    // Main loop -> engine (approval responses)
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(32);

    // ── Event loop ───────────────────────────────────────────

    let mut renderer = UiRenderer::new();
    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;

    loop {
        // ── Phase 1: Wait for input ──────────────────────────

        let input = if let Some(cmd) = pending_command.take() {
            cmd
        } else {
            let model = config.model.clone();
            let mode_clone = shared_mode.clone();

            match tokio::task::spawn_blocking(move || read_input(&model, &mode_clone)).await? {
                Ok(InputResult::Line(text)) => {
                    // Echo the prompt + input to scrollback
                    let mode = approval::read_mode(&shared_mode);
                    let prompt = repl::format_prompt(&config.model, mode);
                    println!("{prompt}{text}");
                    text
                }
                Ok(InputResult::Eof) => {
                    println!("\x1b[36m\u{1f43b} Goodbye!\x1b[0m");
                    break;
                }
                Err(e) => {
                    eprintln!("Input error: {e}");
                    break;
                }
            }
        };

        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        // ── Phase 2: Handle slash commands ───────────────────

        if input.starts_with('/') {
            match repl::handle_command(&input, &config, &provider).await {
                ReplAction::Quit => {
                    println!("\x1b[36m\u{1f43b} Goodbye!\x1b[0m");
                    break;
                }
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
                    continue;
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
                    continue;
                }
                ReplAction::SetupProvider(ptype, base_url) => {
                    crate::commands::handle_setup_provider(&mut config, &provider, ptype, base_url)
                        .await;
                    continue;
                }
                ReplAction::PickProvider => {
                    crate::commands::handle_pick_provider(&mut config, &provider).await;
                    continue;
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
                        pending_command = Some(cmd.to_string());
                    }
                    println!();
                    println!(
                        "  \x1b[90mTips: @file to attach context \u{00b7} Shift+Tab to cycle mode \u{00b7} Ctrl+C to cancel \u{00b7} Ctrl+D to exit\x1b[0m"
                    );
                    continue;
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
                    continue;
                }
                ReplAction::ListSessions => {
                    match session.db.list_sessions(10, &project_root).await {
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
                    continue;
                }
                ReplAction::DeleteSession(ref id) => {
                    if id == &session.id {
                        println!("  \x1b[31mCannot delete the current session.\x1b[0m");
                    } else {
                        match session.db.list_sessions(100, &project_root).await {
                            Ok(sessions) => {
                                let matches: Vec<_> =
                                    sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                                match matches.len() {
                                    0 => println!(
                                        "  \x1b[31mNo session found matching '{id}'.\x1b[0m"
                                    ),
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
                    continue;
                }
                ReplAction::ResumeSession(ref id) => {
                    if session.id.starts_with(id) {
                        println!("  \x1b[90mAlready in this session.\x1b[0m");
                    } else {
                        match session.db.list_sessions(100, &project_root).await {
                            Ok(sessions) => {
                                let matches: Vec<_> =
                                    sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                                match matches.len() {
                                    0 => println!(
                                        "  \x1b[31mNo session found matching '{id}'.\x1b[0m"
                                    ),
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
                    continue;
                }
                ReplAction::InjectPrompt(prompt) => {
                    pending_command = Some(prompt);
                    continue;
                }
                ReplAction::Compact => {
                    crate::commands::handle_compact(
                        &session.db,
                        &session.id,
                        &config,
                        &provider,
                        false,
                    )
                    .await;
                    continue;
                }
                ReplAction::McpCommand(ref args) => {
                    crate::commands::handle_mcp_command(args, &agent.mcp_registry, &project_root)
                        .await;
                    continue;
                }
                ReplAction::SetTrust(mode_name) => {
                    let new_mode = if let Some(ref name) = mode_name {
                        ApprovalMode::parse(name)
                    } else {
                        crate::commands::pick_trust_mode(approval::read_mode(&shared_mode))
                    };
                    if let Some(m) = new_mode {
                        approval::set_mode(&shared_mode, m);
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
                    continue;
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
                    continue;
                }
                ReplAction::Verbose(v) => {
                    renderer.verbose = match v {
                        Some(val) => val,
                        None => !renderer.verbose,
                    };
                    let state = if renderer.verbose { "on" } else { "off" };
                    println!("  \x1b[36mVerbose tool output: {state}\x1b[0m");
                    continue;
                }
                ReplAction::Handled => continue,
                ReplAction::NotACommand => {}
            }
        }

        // ── Phase 3: Prepare and run inference turn ──────────

        // Process @file references
        let processed = input::process_input(&input, &project_root);
        if !processed.images.is_empty() {
            for (i, _img) in processed.images.iter().enumerate() {
                println!("  \x1b[35m\u{1f5bc} Image {}\x1b[0m", i + 1);
            }
        }

        let user_message =
            if let Some(context) = input::format_context_files(&processed.context_files) {
                if !processed.context_files.is_empty() {
                    for f in &processed.context_files {
                        println!("  \x1b[36m\u{1f4ce} {}\x1b[0m", f.path);
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

        // Sync session state
        session.mode = approval::read_mode(&shared_mode);
        session.update_provider(&config);

        // Create a channel-forwarding sink for this turn
        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());

        // Clone the cancel token so we can trigger it from the Ctrl+C branch
        let cancel_token = session.cancel.clone();

        // Run turn
        {
            let turn = session.run_turn(
                &config,
                pending_images,
                &cli_sink,
                &mut cmd_rx,
                &cli_loop_continue_prompt,
            );
            tokio::pin!(turn);

            loop {
                tokio::select! {
                    result = &mut turn => {
                        if let Err(e) = result {
                            println!("\n  \x1b[31m\u{2717} Turn failed: {e}\x1b[0m");
                        }
                        break;
                    }
                    Some(ui_event) = ui_rx.recv() => {
                        match ui_event {
                            UiEvent::Engine(EngineEvent::ApprovalRequest {
                                id,
                                tool_name,
                                detail,
                                preview,
                                whitelist_hint,
                            }) => {
                                if preview.is_some() {
                                    renderer.preview_shown = true;
                                }
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
                            }
                            UiEvent::Engine(ref event) => {
                                renderer.render(event.clone());
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        if crate::interrupt::handle_sigint() {
                            eprintln!("\n\x1b[31mForce quit.\x1b[0m");
                            std::process::exit(130);
                        }
                        cancel_token.cancel();
                    }
                }
            }
        }

        // Drain remaining UI events
        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
            renderer.render(e);
        }
        renderer.stop_spinner();

        crate::interrupt::reset();
        session.cancel = tokio_util::sync::CancellationToken::new();

        // Auto-compact when context window gets crowded
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
                        println!();
                        println!(
                            "  \x1b[33m\u{1f43b} Context at {ctx_pct}% \u{2014} deferring compact (tool calls pending)\x1b[0m"
                        );
                        silent_compact_deferred = true;
                    }
                } else {
                    silent_compact_deferred = false;
                    println!();
                    println!(
                        "  \x1b[36m\u{1f43b} Context at {ctx_pct}% \u{2014} auto-compacting...\x1b[0m"
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

    // Shut down MCP servers
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

// ── Utilities ─────────────────────────────────────────────────

/// CLI implementation of the loop-continue prompt.
/// Shows a terminal select widget when the hard cap is hit.
fn cli_loop_continue_prompt(
    cap: u32,
    recent_names: &[String],
) -> koda_core::loop_guard::LoopContinuation {
    use koda_core::loop_guard::LoopContinuation;

    println!("\n  \x1b[33m\u{26a0}  Hard cap reached ({cap} iterations).\x1b[0m");

    if !recent_names.is_empty() {
        println!("  Last tool calls:");
        for name in recent_names {
            println!("    \x1b[90m\u{25cf}\x1b[0m {name}");
        }
    }
    println!();

    let options = vec![
        SelectOption::new("Stop", "End the task here"),
        SelectOption::new("+50 more", "Continue for 50 more iterations"),
        SelectOption::new("+200 more", "Continue for 200 more iterations"),
    ];

    match crate::tui::select("Continue?", &options, 0) {
        Ok(Some(1)) => LoopContinuation::Continue50,
        Ok(Some(2)) => LoopContinuation::Continue200,
        _ => LoopContinuation::Stop,
    }
}
