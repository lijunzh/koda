//! The main application entry points: interactive REPL and headless mode.
//!
//! Handles user input, command dispatch, and delegates to the inference engine.

use crate::input::{self, KodaHelper};
use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::db::{Database, Role};
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;

use crate::repl::{self, ReplAction};
use crate::tui::{self, SelectOption};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Run the main interactive event loop.
pub async fn run(
    project_root: PathBuf,
    mut config: KodaConfig,
    db: Database,
    session_id: String,
    version_check: tokio::task::JoinHandle<Option<String>>,
) -> Result<()> {
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

    let mut pending_command: Option<String> = None;
    let mut silent_compact_deferred = false;

    // Channel for approval responses from CLI → engine
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<koda_core::engine::EngineCommand>(32);
    let cli_sink = crate::sink::CliSink::new(cmd_tx);

    loop {
        let input = if let Some(cmd) = pending_command.take() {
            cmd
        } else {
            let prompt = repl::format_prompt(&config.model, approval::read_mode(&shared_mode));
            match rl.readline(&prompt) {
                Ok(line) => line,
                Err(
                    rustyline::error::ReadlineError::Interrupted
                    | rustyline::error::ReadlineError::Eof,
                ) => break,
                Err(e) => return Err(e.into()),
            }
        };

        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        let _ = rl.add_history_entry(&input);

        // Handle REPL commands
        if input.starts_with('/') {
            match repl::handle_command(&input, &config, &provider).await {
                ReplAction::Quit => {
                    println!("\x1b[36m\u{1f43b} Goodbye!\x1b[0m");
                    break;
                }
                ReplAction::SwitchModel(model) => {
                    config.model = model.clone();
                    config.model_settings.model = model.clone();
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
                            if let Some(h) = rl.helper_mut() {
                                h.model_names = models.iter().map(|m| m.id.clone()).collect();
                            }
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
                    crate::commands::handle_setup_provider(
                        &mut config,
                        &provider,
                        &mut rl,
                        ptype,
                        base_url,
                    )
                    .await;
                    continue;
                }
                ReplAction::PickProvider => {
                    crate::commands::handle_pick_provider(&mut config, &provider, &mut rl).await;
                    continue;
                }

                ReplAction::ShowHelp => {
                    let commands = [
                        ("/agent", "List available sub-agents"),
                        ("/compact", "Summarize conversation to reclaim context"),
                        ("/cost", "Show token usage for this session"),
                        ("/diff", "Show git diff / review / commit message"),
                        ("/mcp", "MCP servers: status / add / remove / restart"),
                        ("/memory", "View/save project & global memory"),
                        ("/model", "Pick a model interactively"),
                        ("/provider", "Switch LLM provider"),
                        ("/sessions", "List/resume/delete sessions"),
                        ("/trust", "Set approval mode (always / auto / never)"),
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
                        "  \x1b[90mTips: @file to attach context \u{00b7} Ctrl+C to clear input \u{00b7} Ctrl+D to exit\x1b[0m"
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
                        Ok(sessions) => {
                            println!();
                            println!("  \x1b[1m\u{1f43b} Recent Sessions\x1b[0m");
                            println!();
                            if sessions.is_empty() {
                                println!("  \x1b[90mNo sessions found.\x1b[0m");
                            } else {
                                for s in &sessions {
                                    let marker = if s.id == session.id {
                                        " \x1b[32m← current\x1b[0m"
                                    } else {
                                        ""
                                    };
                                    println!(
                                        "  \x1b[36m{}\x1b[0m  \x1b[90m{}  {}  {} msgs  {}k tokens\x1b[0m{}",
                                        &s.id[..8],
                                        s.created_at,
                                        s.agent_name,
                                        s.message_count,
                                        s.total_tokens / 1000,
                                        marker,
                                    );
                                }
                            }
                            println!();
                            println!("  \x1b[90mResume: koda --session <id>\x1b[0m");
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
                                            Err(e) => println!("  \x1b[31mError: {e}\x1b[0m"),
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
                ReplAction::InjectPrompt(prompt) => {
                    // Treat injected prompt as user input (used by /diff review, /diff commit)
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
                        // Explicit: /trust yolo
                        ApprovalMode::parse(name)
                    } else {
                        // Interactive picker
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
                ReplAction::Handled => continue,
                ReplAction::NotACommand => {}
            }
        }

        // Process @file references
        let processed = input::process_input(&input, &project_root);
        // Show attached images
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

        session
            .db
            .insert_message(
                &session.id,
                &Role::User,
                Some(&user_message),
                None,
                None,
                None,
            )
            .await?;

        // Pass images to inference (they're not stored in DB, only used in-flight)
        let pending_images = if processed.images.is_empty() {
            None
        } else {
            Some(processed.images)
        };

        // Sync session state from REPL changes (model/provider switching)
        session.mode = approval::read_mode(&shared_mode);
        session.update_provider(&config);
        session
            .run_turn(
                &config,
                pending_images,
                &cli_sink,
                &mut cmd_rx,
                &crate::app::cli_loop_continue_prompt,
            )
            .await?;

        // Auto-compact when context window gets crowded
        if config.auto_compact_threshold > 0 {
            let ctx_pct = koda_core::context::percentage();
            if ctx_pct >= config.auto_compact_threshold {
                // Fix 5: Defer compaction if tool calls are still pending
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

    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = rl.save_history(&history_path);

    // Shut down MCP servers
    {
        let mut mcp = agent.mcp_registry.write().await;
        mcp.shutdown();
    }

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
