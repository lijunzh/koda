//! Slash command handler for the TUI event loop.
//!
//! All output rendered through `tui_output::emit_line()` with native
//! ratatui `Line`/`Span` styling. Stays in raw mode.

use crate::repl::ReplAction;
use crate::select_menu::{self, SelectOption};
use crate::tui_output;
use crate::tui_render::TuiRenderer;

use koda_core::agent::KodaAgent;
use koda_core::approval::{self, ApprovalMode};
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::RwLock;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

pub enum SlashAction {
    Continue,
    Quit,
}

// ── Style helpers ────────────────────────────────────────────

const DIM: Style = Style::new().fg(Color::DarkGray);
const ERR: Style = Style::new().fg(Color::Red);
const OK: Style = Style::new().fg(Color::Green);
const CYAN: Style = Style::new().fg(Color::Cyan);
const WARN: Style = Style::new().fg(Color::Yellow);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

fn ok_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![Span::styled("  \u{2713} ", OK), Span::raw(msg)]),
    );
}

fn err_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::styled("  \u{2717} ", ERR),
            Span::styled(msg, ERR),
        ]),
    );
}

fn dim_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(terminal, Line::styled(format!("  {msg}"), DIM));
}

fn warn_msg(terminal: &mut Term, msg: String) {
    tui_output::emit_line(
        terminal,
        Line::from(vec![
            Span::styled("  \u{26a0} ", WARN),
            Span::styled(msg, WARN),
        ]),
    );
}

// ── Main handler ────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn handle_slash_command(
    terminal: &mut Term,
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
            save_provider(config);
            ok_msg(terminal, format!("Model set to: {model}"));
            SlashAction::Continue
        }
        ReplAction::PickModel => {
            handle_pick_model(terminal, config, provider).await;
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
            handle_help(terminal, pending_command);
            SlashAction::Continue
        }
        ReplAction::ShowCost => {
            handle_cost(terminal, session, config).await;
            SlashAction::Continue
        }
        ReplAction::ListSessions => {
            handle_list_sessions(terminal, session, project_root).await;
            SlashAction::Continue
        }
        ReplAction::DeleteSession(ref id) => {
            handle_delete_session(terminal, session, id, project_root).await;
            SlashAction::Continue
        }
        ReplAction::ResumeSession(ref id) => {
            handle_resume_session(terminal, session, id, project_root).await;
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
            handle_trust(terminal, mode_name, shared_mode);
            SlashAction::Continue
        }
        ReplAction::Expand(n) => {
            handle_expand(terminal, renderer, n);
            SlashAction::Continue
        }
        ReplAction::Verbose(v) => {
            renderer.verbose = match v {
                Some(val) => val,
                None => !renderer.verbose,
            };
            let state = if renderer.verbose { "on" } else { "off" };
            tui_output::emit_line(
                terminal,
                Line::styled(format!("  Verbose tool output: {state}"), CYAN),
            );
            SlashAction::Continue
        }
        ReplAction::Handled => SlashAction::Continue,
        ReplAction::NotACommand => SlashAction::Continue,
    }
}

// ── Sub-handlers ───────────────────────────────────────────

async fn handle_pick_model(
    terminal: &mut Term,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    let prov = provider.read().await;
    match prov.list_models().await {
        Ok(models) if models.is_empty() => {
            warn_msg(
                terminal,
                format!("No models available from {}", prov.provider_name()),
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
            match select_menu::select_raw("\u{1f43b} Select a model", &options, current_idx) {
                Ok(Some(idx)) => {
                    config.model = models[idx].id.clone();
                    config.model_settings.model = config.model.clone();
                    save_provider(config);
                    ok_msg(terminal, format!("Model set to: {}", config.model));
                }
                Ok(None) => dim_msg(terminal, "Cancelled.".into()),
                Err(e) => err_msg(terminal, format!("TUI error: {e}")),
            }
        }
        Err(e) => err_msg(terminal, format!("Failed to list models: {e}")),
    }
}

fn handle_help(terminal: &mut Term, pending_command: &mut Option<String>) {
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
    if let Ok(Some(idx)) = select_menu::select_raw("\u{1f43b} Commands", &options, 0) {
        let (cmd, _) = commands[idx];
        *pending_command = Some(cmd.to_string());
    }
    tui_output::emit_blank(terminal);
    dim_msg(
        terminal,
        "Tips: @file to attach context \u{00b7} Shift+Tab to cycle mode \u{00b7} Ctrl+C to cancel \u{00b7} Ctrl+D to exit".into(),
    );
}

async fn handle_cost(terminal: &mut Term, session: &KodaSession, config: &KodaConfig) {
    match session.db.session_token_usage(&session.id).await {
        Ok(u) => {
            let total = u.prompt_tokens
                + u.completion_tokens
                + u.cache_read_tokens
                + u.cache_creation_tokens;
            tui_output::emit_blank(terminal);
            tui_output::emit_line(terminal, Line::styled("  \u{1f43b} Session Cost", BOLD));
            tui_output::emit_blank(terminal);

            let mut rows = vec![
                ("Prompt tokens:", format!("{:>8}", u.prompt_tokens), CYAN),
                (
                    "Completion tokens:",
                    format!("{:>8}", u.completion_tokens),
                    CYAN,
                ),
            ];
            if u.cache_read_tokens > 0 {
                rows.push((
                    "Cache read tokens:",
                    format!("{:>8}", u.cache_read_tokens),
                    OK,
                ));
            }
            if u.cache_creation_tokens > 0 {
                rows.push((
                    "Cache write tokens:",
                    format!("{:>8}", u.cache_creation_tokens),
                    WARN,
                ));
            }
            if u.thinking_tokens > 0 {
                rows.push((
                    "Thinking tokens:",
                    format!("{:>8}", u.thinking_tokens),
                    Style::new().fg(Color::Magenta),
                ));
            }
            rows.push(("Total tokens:", format!("{total:>8}"), BOLD));
            rows.push(("API calls:", format!("{:>8}", u.api_calls), DIM));

            for (label, value, style) in &rows {
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw(format!("  {label:<21}")),
                        Span::styled(value, *style),
                    ]),
                );
            }
            tui_output::emit_blank(terminal);
            dim_msg(terminal, format!("Model: {}", config.model));
            dim_msg(terminal, format!("Provider: {}", config.provider_type));
        }
        Err(e) => err_msg(terminal, format!("Error: {e}")),
    }
}

async fn handle_list_sessions(
    terminal: &mut Term,
    session: &mut KodaSession,
    project_root: &std::path::Path,
) {
    match session.db.list_sessions(10, project_root).await {
        Ok(sessions) if sessions.is_empty() => {
            dim_msg(terminal, "No other sessions found.".into());
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
            match select_menu::select_raw("\u{1f43b} Sessions", &options, current_idx) {
                Ok(Some(idx)) => {
                    let target = &sessions[idx];
                    if target.id == session.id {
                        dim_msg(terminal, "Already in this session.".into());
                    } else {
                        session.id = target.id.clone();
                        tui_output::emit_line(
                            terminal,
                            Line::from(vec![
                                Span::styled("  \u{2713} ", OK),
                                Span::raw("Resumed session "),
                                Span::styled(&target.id[..8], CYAN),
                                Span::styled(
                                    format!(
                                        "  {}  {} msgs",
                                        target.created_at, target.message_count
                                    ),
                                    DIM,
                                ),
                            ]),
                        );
                    }
                }
                Ok(None) => dim_msg(terminal, "Cancelled.".into()),
                Err(e) => err_msg(terminal, format!("TUI error: {e}")),
            }
            dim_msg(terminal, "Delete: /sessions delete <id>".into());
        }
        Err(e) => err_msg(terminal, format!("Error: {e}")),
    }
}

async fn handle_delete_session(
    terminal: &mut Term,
    session: &KodaSession,
    id: &str,
    project_root: &std::path::Path,
) {
    if id == session.id {
        err_msg(terminal, "Cannot delete the current session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => err_msg(terminal, format!("No session found matching '{id}'.")),
                    1 => {
                        let full_id = &matches[0].id;
                        match session.db.delete_session(full_id).await {
                            Ok(true) => {
                                ok_msg(terminal, format!("Deleted session {}", &full_id[..8]))
                            }
                            Ok(false) => err_msg(terminal, "Session not found.".into()),
                            Err(e) => err_msg(terminal, format!("Error: {e}")),
                        }
                    }
                    n => err_msg(
                        terminal,
                        format!("Ambiguous: '{id}' matches {n} sessions. Be more specific."),
                    ),
                }
            }
            Err(e) => err_msg(terminal, format!("Error: {e}")),
        }
    }
}

async fn handle_resume_session(
    terminal: &mut Term,
    session: &mut KodaSession,
    id: &str,
    project_root: &std::path::Path,
) {
    if session.id.starts_with(id) {
        dim_msg(terminal, "Already in this session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => err_msg(terminal, format!("No session found matching '{id}'.")),
                    1 => {
                        let target = &matches[0];
                        session.id = target.id.clone();
                        tui_output::emit_line(
                            terminal,
                            Line::from(vec![
                                Span::styled("  \u{2713} ", OK),
                                Span::raw("Resumed session "),
                                Span::styled(&target.id[..8], CYAN),
                                Span::styled(
                                    format!(
                                        "  {}  {} msgs",
                                        target.created_at, target.message_count
                                    ),
                                    DIM,
                                ),
                            ]),
                        );
                    }
                    n => err_msg(
                        terminal,
                        format!("Ambiguous: '{id}' matches {n} sessions. Be more specific."),
                    ),
                }
            }
            Err(e) => err_msg(terminal, format!("Error: {e}")),
        }
    }
}

fn handle_trust(
    terminal: &mut Term,
    mode_name: Option<String>,
    shared_mode: &approval::SharedMode,
) {
    let new_mode = if let Some(ref name) = mode_name {
        ApprovalMode::parse(name)
    } else {
        crate::commands::pick_trust_mode(approval::read_mode(shared_mode))
    };
    if let Some(m) = new_mode {
        approval::set_mode(shared_mode, m);
        tui_output::emit_line(
            terminal,
            Line::from(vec![
                Span::styled("  \u{2713} ", OK),
                Span::raw("Trust: "),
                Span::styled(m.label(), BOLD),
                Span::raw(format!(" \u{2014} {}", m.description())),
            ]),
        );
    } else if let Some(ref name) = mode_name {
        err_msg(
            terminal,
            format!("Unknown trust level '{name}'. Use: plan, normal, yolo"),
        );
    }
}

fn handle_expand(terminal: &mut Term, renderer: &TuiRenderer, n: usize) {
    match renderer.tool_history.get(n) {
        Some(record) => {
            tui_output::emit_blank(terminal);
            tui_output::emit_line(
                terminal,
                Line::from(vec![
                    Span::styled(format!("  \u{1f50d} Expand: {}", record.tool_name), BOLD),
                    Span::styled(format!(" ({} lines)", record.output.lines().count()), DIM),
                ]),
            );
            for line in record.output.lines() {
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::styled("  \u{2502} ", DIM),
                        Span::raw(line.to_string()),
                    ]),
                );
            }
            tui_output::emit_blank(terminal);
        }
        None => {
            let total = renderer.tool_history.len();
            if total == 0 {
                dim_msg(terminal, "No tool outputs recorded yet.".into());
            } else {
                warn_msg(
                    terminal,
                    format!(
                        "No tool output #{n}. Have {total} recorded (use /expand 1\u{2013}{total})."
                    ),
                );
            }
        }
    }
}

fn save_provider(config: &KodaConfig) {
    let mut s = koda_core::approval::Settings::load();
    let _ = s.save_last_provider(
        &config.provider_type.to_string(),
        &config.base_url,
        &config.model,
    );
}
