//! The main application entry points: interactive REPL and headless mode.
//!
//! The REPL uses an async event loop with readline on a dedicated OS thread.
//! The engine runs as a pinned future in `tokio::select!`, allowing concurrent
//! event rendering while the inference turn is active.

use crate::input::{self, KodaHelper};
use crate::sink::UiEvent;
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;

use crate::repl::{self, ReplAction};
use crate::sink::UiRenderer;
use crate::tui::{self, SelectOption};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

// ── Readline ↔ main loop protocol ────────────────────────────

/// Events sent from the readline thread to the main async loop.
enum InputEvent {
    Line(String),
    Eof,
}

/// Commands sent from the main async loop to the readline thread.
enum ReadlineCommand {
    /// Request a readline with the given prompt string.
    ReadLine(String),
    /// Update the model names available for tab-completion.
    UpdateModelNames(Vec<String>),
    /// Shut down the readline thread.
    Shutdown,
}

// ── Readline thread ──────────────────────────────────────────

/// Runs `rustyline` on a dedicated OS thread so it never blocks the
/// tokio runtime.
///
/// - `cmd_rx`: receives commands from the main loop (std channel — blocking OK).
/// - `input_tx`: sends user input back to the main loop (tokio channel).
fn readline_thread(
    mut rl: rustyline::Editor<KodaHelper, rustyline::history::DefaultHistory>,
    cmd_rx: std::sync::mpsc::Receiver<ReadlineCommand>,
    input_tx: tokio::sync::mpsc::Sender<InputEvent>,
) {
    loop {
        match cmd_rx.recv() {
            Ok(ReadlineCommand::ReadLine(prompt)) => {
                match rl.readline(&prompt) {
                    Ok(line) => {
                        let _ = rl.add_history_entry(&line);
                        let _ = rl.save_history(&history_file_path());
                        // blocking_send is fine — we're on an OS thread.
                        let _ = input_tx.blocking_send(InputEvent::Line(line));
                    }
                    Err(
                        rustyline::error::ReadlineError::Interrupted
                        | rustyline::error::ReadlineError::Eof,
                    ) => {
                        let _ = input_tx.blocking_send(InputEvent::Eof);
                        break;
                    }
                    Err(_) => break,
                }
            }
            Ok(ReadlineCommand::UpdateModelNames(names)) => {
                if let Some(h) = rl.helper_mut() {
                    h.model_names = names;
                }
            }
            Ok(ReadlineCommand::Shutdown) | Err(_) => {
                let _ = rl.save_history(&history_file_path());
                break;
            }
        }
    }
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

// ── Main event loop ──────────────────────────────────────────

/// Run the main interactive event loop.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
) -> Result<()> {
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
    if let Ok(Some(latest)) = version_check.await
        && let Some((current, latest)) = koda_core::version::update_available(&latest)
    {
        let crate_name = koda_core::version::crate_name();
        println!(
            "  \x1b[90m\u{2728} Update available: \x1b[0m\x1b[36m{current}\x1b[0m\x1b[90m \u{2192} \x1b[0m\x1b[32m{latest}\x1b[0m\x1b[90m  (cargo install {crate_name})\x1b[0m"
        );
        println!();
    }

    // Build agent (tools, MCP, system prompt) and session
    let agent = Arc::new(KodaAgent::new(&config, project_root.clone()).await?);

    // Render MCP connection statuses
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

    // REPL with smart completions
    let shared_mode = approval::new_shared_mode(ApprovalMode::Normal);

    let mut helper = KodaHelper::new(project_root.clone(), shared_mode.clone());
    {
        let prov = provider.read().await;
        if let Ok(models) = prov.list_models().await {
            helper.model_names = models.iter().map(|m| m.id.clone()).collect();
        }
    }

    let mut rl = rustyline::Editor::with_config(
        rustyline::Config::builder()
            .completion_type(rustyline::CompletionType::List)
            .build(),
    )?;
    rl.set_helper(Some(helper));

    // Esc clears the current line (no-op when already empty).
    // Ctrl-C clears the line when non-empty, or exits when the buffer is empty.
    rl.bind_sequence(
        rustyline::KeyEvent(rustyline::KeyCode::Esc, rustyline::Modifiers::NONE),
        rustyline::EventHandler::Conditional(Box::new(input::EscClearHandler)),
    );
    rl.bind_sequence(
        rustyline::KeyEvent::ctrl('c'),
        rustyline::EventHandler::Conditional(Box::new(input::CtrlCClearHandler)),
    );

    // Shift+Tab cycles approval mode: Plan → Normal → Yolo
    // Note: rustyline normalizes Shift+Tab to BackTab with NONE modifiers
    rl.bind_sequence(
        rustyline::KeyEvent(rustyline::KeyCode::BackTab, rustyline::Modifiers::NONE),
        rustyline::EventHandler::Conditional(Box::new(input::ShiftTabModeHandler::new(
            shared_mode.clone(),
        ))),
    );

    let history_path = history_file_path();
    if history_path.exists() {
        let _ = rl.load_history(&history_path);
    }

    // ── Channels ─────────────────────────────────────────────

    // Readline thread ↔ main loop
    let (rl_cmd_tx, rl_cmd_rx) = std::sync::mpsc::channel::<ReadlineCommand>();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputEvent>(4);

    // Engine sink → main loop (UI events)
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::channel::<UiEvent>(256);

    // Main loop → engine (approval responses)
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<EngineCommand>(32);

    // ── Spawn readline thread ────────────────────────────────

    std::thread::spawn(move || readline_thread(rl, rl_cmd_rx, input_tx));

    // ── Event loop ───────────────────────────────────────

    let mut renderer = UiRenderer::new();
    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;

    loop {
        // ── Phase 1: Wait for input ──────────────────────────
        let input = if let Some(cmd) = pending_command.take() {
            cmd
        } else {
            let prompt = repl::format_prompt(&config.model, approval::read_mode(&shared_mode));
            if rl_cmd_tx.send(ReadlineCommand::ReadLine(prompt)).is_err() {
                break; // readline thread died
            }
            match input_rx.recv().await {
                Some(InputEvent::Line(line)) => line,
                Some(InputEvent::Eof) | None => break,
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
                    // Persist for next startup
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
                            let names: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
                            let _ = rl_cmd_tx.send(ReadlineCommand::UpdateModelNames(names));
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
                        "  \x1b[90mTips: @file to attach context \u{00b7} type while model runs \u{00b7} Ctrl+C to cancel \u{00b7} Ctrl+D to exit\x1b[0m"
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
                        // Match by prefix
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
                                    "  \x1b[33mNo tool output #{n}. Have {total} recorded (use /expand 1–{total}).\x1b[0m"
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

        // Sync session state from REPL changes (model/provider switching)
        session.mode = approval::read_mode(&shared_mode);
        session.update_provider(&config);

        // Create a channel-forwarding sink for this turn
        let cli_sink = crate::sink::CliSink::channel(ui_tx.clone());

        // Clone the cancel token so we can trigger it from the Ctrl+C branch
        // while `session` is borrowed by `run_turn`.
        let cancel_token = session.cancel.clone();

        // Run turn in a scoped block so borrows are released on completion.
        {
            let turn = session.run_turn(
                &config,
                pending_images,
                &cli_sink,
                &mut cmd_rx,
                &crate::app::cli_loop_continue_prompt,
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
                                // Track that a preview was shown for this tool
                                if preview.is_some() {
                                    renderer.preview_shown = true;
                                }
                                // Readline thread is paused — stdin is free for TUI.
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
        // Borrows on session/config/cmd_rx released here.

        // Drain remaining UI events (e.g., SpinnerStop after Ctrl+C interrupt).
        // Without this, the spinner task can keep running and overwrite the prompt.
        while let Ok(UiEvent::Engine(e)) = ui_rx.try_recv() {
            renderer.render(e);
        }
        // Safety net: ensure the spinner is stopped even if SpinnerStop was lost.
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
                            "  \x1b[33m\u{1f43b} Context at {ctx_pct}% — deferring compact (tool calls pending)\x1b[0m"
                        );
                        silent_compact_deferred = true;
                    }
                } else {
                    silent_compact_deferred = false;
                    println!();
                    println!(
                        "  \x1b[36m\u{1f43b} Context at {ctx_pct}% — auto-compacting...\x1b[0m"
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

    // Shut down readline thread
    let _ = rl_cmd_tx.send(ReadlineCommand::Shutdown);

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

fn history_file_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.config")))
        .or_else(|_| std::env::var("USERPROFILE").map(|h| format!("{h}/.config")))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(config_dir).join("koda").join("history")
}

/// CLI implementation of the loop-continue prompt.
/// Shows a terminal select widget when the hard cap is hit.
pub fn cli_loop_continue_prompt(
    cap: u32,
    recent_names: &[String],
) -> koda_core::loop_guard::LoopContinuation {
    use crate::tui::SelectOption;
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

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::config::ProviderType;

    #[test]
    fn test_create_provider_openai() {
        let config = KodaConfig::default_for_testing(ProviderType::OpenAI);
        let provider = crate::commands::create_provider(&config);
        assert_eq!(provider.provider_name(), "openai-compat");
    }

    #[test]
    fn test_create_provider_anthropic() {
        let config = KodaConfig::default_for_testing(ProviderType::Anthropic);
        let provider = crate::commands::create_provider(&config);
        assert_eq!(provider.provider_name(), "anthropic");
    }

    #[test]
    fn test_create_provider_lmstudio() {
        let config = KodaConfig::default_for_testing(ProviderType::LMStudio);
        let provider = crate::commands::create_provider(&config);
        assert_eq!(provider.provider_name(), "openai-compat");
    }

    #[test]
    fn test_create_provider_gemini() {
        let config = KodaConfig::default_for_testing(ProviderType::Gemini);
        let provider = crate::commands::create_provider(&config);
        assert_eq!(provider.provider_name(), "gemini");
    }

    #[test]
    fn test_history_file_path() {
        let path = history_file_path();
        assert!(path.to_string_lossy().contains("koda"));
        assert!(path.to_string_lossy().contains("history"));
    }
}
