//! Slash command handler for the TUI event loop.
//!
//! Extracted from `tui_app.rs` to keep file sizes under 600 lines.
//! These commands run with raw mode temporarily disabled, so they
//! can use `println!` and the legacy `tui::select()` widget.

use crate::repl::ReplAction;
use crate::tui::{self, SelectOption};
use crate::tui_render::TuiRenderer;

use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use std::sync::Arc;
use tokio::sync::RwLock;

pub enum SlashAction {
    Continue,
    Quit,
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_slash_command(
    input: &str,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    session: &mut KodaSession,
    shared_mode: &approval::SharedMode,
    renderer: &mut TuiRenderer,
    project_root: &std::path::Path,
    agent: &Arc<KodaAgent>,
    pending_command: &mut Option<String>,
) -> SlashAction {
    match crate::repl::handle_command(input, config, provider).await {
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
                    // Print expanded tool output (legacy println, tracked in #84)
                    println!(
                        "\n\x1b[1m\u{1f50d} Expand: {}\x1b[0m ({} lines)",
                        record.tool_name,
                        record.output.lines().count()
                    );
                    for line in record.output.lines() {
                        println!("  \x1b[90m\u{2502}\x1b[0m {line}");
                    }
                    println!();
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
