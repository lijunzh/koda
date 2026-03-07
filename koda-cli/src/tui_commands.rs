//! Slash command handler for the TUI event loop.
//!
//! All output rendered through `tui_output::write_line(&)` with native
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
    style::{Color, Style},
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::RwLock;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

pub enum SlashAction {
    Continue,
    Quit,
}

// ── Style helpers (crossterm direct writes) ─────────────────
//
// All slash command output goes through write_line/write_blank
// (crossterm \r\n), NOT insert_before. This avoids cursor conflicts
// with select_inline which also uses crossterm.

use tui_output::{BOLD, CYAN, DIM, GREEN as OK, YELLOW as WARN};
use tui_output::{dim_msg, err_msg, ok_msg, warn_msg};

// ── Main handler ────────────────────────────────────────────

#[allow(clippy::too_many_arguments, unused_variables)]
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
            config.recalculate_model_derived();
            // Query actual capabilities from the API
            {
                let prov = provider.read().await;
                if let Ok(caps) = prov.model_capabilities(&model).await {
                    config.apply_provider_capabilities(&caps);
                }
            }
            crate::tui_wizards::save_provider(config);
            ok_msg(format!("Model set to: {model}"));
            SlashAction::Continue
        }
        ReplAction::PickModel => {
            handle_pick_model(terminal, config, provider).await;
            SlashAction::Continue
        }
        ReplAction::SetupProvider(ptype, base_url) => {
            crate::tui_wizards::handle_setup_provider(terminal, config, provider, ptype, base_url)
                .await;
            SlashAction::Continue
        }
        ReplAction::PickProvider => {
            crate::tui_wizards::handle_pick_provider(terminal, config, provider).await;
            SlashAction::Continue
        }
        ReplAction::ShowHelp => {
            handle_help(terminal, pending_command);
            // If /help selected a command, execute it right away
            if let Some(cmd) = pending_command.take() {
                return Box::pin(handle_slash_command(
                    terminal,
                    &cmd,
                    config,
                    provider,
                    session,
                    shared_mode,
                    renderer,
                    project_root,
                    agent,
                    pending_command,
                ))
                .await;
            }
            SlashAction::Continue
        }
        ReplAction::ShowCost => {
            handle_cost(terminal, session, config).await;
            SlashAction::Continue
        }
        ReplAction::Undo => {
            match agent.tools.undo.lock() {
                Ok(mut undo) => match undo.undo() {
                    Some(summary) => ok_msg(summary),
                    None => warn_msg("Nothing to undo.".to_string()),
                },
                Err(e) => err_msg(format!("Undo error: {e}")),
            }
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
            crate::tui_wizards::handle_compact(terminal, session, config, provider).await;
            SlashAction::Continue
        }
        ReplAction::McpCommand(ref args) => {
            crate::tui_wizards::handle_mcp(terminal, args, &agent.mcp_registry, project_root).await;
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
            tui_output::write_line(&Line::styled(
                format!("  Verbose tool output: {state}"),
                CYAN,
            ));
            SlashAction::Continue
        }
        ReplAction::ListAgents => {
            crate::tui_wizards::handle_list_agents(terminal, project_root);
            SlashAction::Continue
        }
        ReplAction::ShowDiff => {
            crate::tui_wizards::handle_diff(terminal);
            SlashAction::Continue
        }
        ReplAction::MemoryCommand(ref arg) => {
            crate::tui_wizards::handle_memory(terminal, arg.as_deref(), project_root);
            SlashAction::Continue
        }
        ReplAction::Handled => SlashAction::Continue,
        ReplAction::NotACommand => SlashAction::Continue,
    }
}

// ── Sub-handlers ───────────────────────────────────────────

#[allow(unused_variables)]
async fn handle_pick_model(
    terminal: &mut Term,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) {
    let prov = provider.read().await;
    match prov.list_models().await {
        Ok(models) if models.is_empty() => {
            warn_msg(format!("No models available from {}", prov.provider_name()));
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
            match select_menu::select_inline(
                terminal,
                "\u{1f43b} Select a model",
                &options,
                current_idx,
            ) {
                Ok(Some(idx)) => {
                    config.model = models[idx].id.clone();
                    config.model_settings.model = config.model.clone();
                    config.recalculate_model_derived();
                    // Query actual capabilities from the API (re-acquire lock)
                    {
                        let prov = provider.read().await;
                        if let Ok(caps) = prov.model_capabilities(&config.model).await {
                            config.apply_provider_capabilities(&caps);
                        }
                    }
                    crate::tui_wizards::save_provider(config);
                    ok_msg(format!("Model set to: {}", config.model));
                }
                Ok(None) => dim_msg("Cancelled.".into()),
                Err(e) => err_msg(format!("TUI error: {e}")),
            }
        }
        Err(e) => err_msg(format!("Failed to list models: {e}")),
    }
}

#[allow(unused_variables)]
fn handle_help(terminal: &mut Term, pending_command: &mut Option<String>) {
    // Emit tips via crossterm (same rendering system as select_inline)
    // so cursor math stays consistent — no insert_before here.
    {
        use crossterm::{
            execute,
            style::{Color, Print, ResetColor, SetForegroundColor},
        };
        let mut stdout = std::io::stdout();
        execute!(
            stdout,
            Print("\r\n  "),
            SetForegroundColor(Color::DarkGrey),
            Print(
                "Tips: @file to attach context \u{00b7} Alt+Enter for newline \u{00b7} Shift+Tab to cycle mode \u{00b7} Ctrl+C to cancel"
            ),
            ResetColor,
            Print("\r\n"),
        )
        .ok();
    }

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
        ("/undo", "Undo last turn's file changes"),
        ("/verbose", "Toggle full tool output (on/off)"),
        ("/exit", "Quit the session"),
    ];
    let options: Vec<SelectOption> = commands
        .iter()
        .map(|(cmd, desc)| SelectOption::new(*cmd, *desc))
        .collect();
    if let Ok(Some(idx)) = select_menu::select_inline(terminal, "\u{1f43b} Commands", &options, 0) {
        let (cmd, _) = commands[idx];
        *pending_command = Some(cmd.to_string());
    }
}

#[allow(unused_variables)]
async fn handle_cost(_terminal: &mut Term, session: &KodaSession, config: &KodaConfig) {
    match session.db.session_token_usage(&session.id).await {
        Ok(u) => {
            let total = u.prompt_tokens
                + u.completion_tokens
                + u.cache_read_tokens
                + u.cache_creation_tokens;
            tui_output::write_blank();
            tui_output::write_line(&Line::styled("  \u{1f43b} Session Cost", BOLD));
            tui_output::write_blank();

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
                tui_output::write_line(&Line::from(vec![
                    Span::raw(format!("  {label:<21}")),
                    Span::styled(value, *style),
                ]));
            }
            tui_output::write_blank();
            dim_msg(format!("Model: {}", config.model));
            dim_msg(format!("Provider: {}", config.provider_type));

            // Per-agent breakdown (if multiple agents used)
            if let Ok(agent_usage) = session.db.session_usage_by_agent(&session.id).await
                && agent_usage.len() > 1
            {
                tui_output::write_blank();
                tui_output::write_line(&Line::styled("  \u{1f4ca} By Agent", BOLD));
                tui_output::write_blank();
                for (agent, au) in &agent_usage {
                    let total = au.prompt_tokens + au.completion_tokens;
                    tui_output::write_line(&Line::from(vec![
                        Span::raw(format!("  {agent:<16}")),
                        Span::styled(format!("{total:>8} tok  ({} calls)", au.api_calls), DIM),
                    ]));
                }
            }
        }
        Err(e) => err_msg(format!("Error: {e}")),
    }
}

#[allow(unused_variables)]
async fn handle_list_sessions(
    terminal: &mut Term,
    session: &mut KodaSession,
    project_root: &std::path::Path,
) {
    match session.db.list_sessions(10, project_root).await {
        Ok(sessions) if sessions.is_empty() => {
            dim_msg("No other sessions found.".into());
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
            match select_menu::select_inline(terminal, "\u{1f43b} Sessions", &options, current_idx)
            {
                Ok(Some(idx)) => {
                    let target = &sessions[idx];
                    if target.id == session.id {
                        dim_msg("Already in this session.".into());
                    } else {
                        session.id = target.id.clone();
                        tui_output::write_line(&Line::from(vec![
                            Span::styled("  \u{2713} ", OK),
                            Span::raw("Resumed session "),
                            Span::styled(&target.id[..8], CYAN),
                            Span::styled(
                                format!("  {}  {} msgs", target.created_at, target.message_count),
                                DIM,
                            ),
                        ]));
                    }
                }
                Ok(None) => dim_msg("Cancelled.".into()),
                Err(e) => err_msg(format!("TUI error: {e}")),
            }
            dim_msg("Delete: /sessions delete <id>".into());
        }
        Err(e) => err_msg(format!("Error: {e}")),
    }
}

#[allow(unused_variables)]
async fn handle_delete_session(
    terminal: &mut Term,
    session: &KodaSession,
    id: &str,
    project_root: &std::path::Path,
) {
    if id == session.id {
        err_msg("Cannot delete the current session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => err_msg(format!("No session found matching '{id}'.")),
                    1 => {
                        let full_id = &matches[0].id;
                        match session.db.delete_session(full_id).await {
                            Ok(true) => ok_msg(format!("Deleted session {}", &full_id[..8])),
                            Ok(false) => err_msg("Session not found.".into()),
                            Err(e) => err_msg(format!("Error: {e}")),
                        }
                    }
                    n => err_msg(format!(
                        "Ambiguous: '{id}' matches {n} sessions. Be more specific."
                    )),
                }
            }
            Err(e) => err_msg(format!("Error: {e}")),
        }
    }
}

#[allow(unused_variables)]
async fn handle_resume_session(
    terminal: &mut Term,
    session: &mut KodaSession,
    id: &str,
    project_root: &std::path::Path,
) {
    if session.id.starts_with(id) {
        dim_msg("Already in this session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => err_msg(format!("No session found matching '{id}'.")),
                    1 => {
                        let target = &matches[0];
                        session.id = target.id.clone();
                        tui_output::write_line(&Line::from(vec![
                            Span::styled("  \u{2713} ", OK),
                            Span::raw("Resumed session "),
                            Span::styled(&target.id[..8], CYAN),
                            Span::styled(
                                format!("  {}  {} msgs", target.created_at, target.message_count),
                                DIM,
                            ),
                        ]));
                    }
                    n => err_msg(format!(
                        "Ambiguous: '{id}' matches {n} sessions. Be more specific."
                    )),
                }
            }
            Err(e) => err_msg(format!("Error: {e}")),
        }
    }
}

#[allow(unused_variables)]
fn handle_trust(
    terminal: &mut Term,
    mode_name: Option<String>,
    shared_mode: &approval::SharedMode,
) {
    let new_mode = if let Some(ref name) = mode_name {
        ApprovalMode::parse(name)
    } else {
        crate::tui_wizards::pick_trust_inline(terminal, approval::read_mode(shared_mode))
    };
    if let Some(m) = new_mode {
        approval::set_mode(shared_mode, m);
        tui_output::write_line(&Line::from(vec![
            Span::styled("  \u{2713} ", OK),
            Span::raw("Trust: "),
            Span::styled(m.label(), BOLD),
            Span::raw(format!(" \u{2014} {}", m.description())),
        ]));
    } else if let Some(ref name) = mode_name {
        err_msg(format!(
            "Unknown trust level '{name}'. Use: plan, normal, yolo"
        ));
    }
}

#[allow(unused_variables)]
fn handle_expand(terminal: &mut Term, renderer: &TuiRenderer, n: usize) {
    match renderer.tool_history.get(n) {
        Some(record) => {
            tui_output::write_blank();
            tui_output::write_line(&Line::from(vec![
                Span::styled(format!("  \u{1f50d} Expand: {}", record.tool_name), BOLD),
                Span::styled(format!(" ({} lines)", record.output.lines().count()), DIM),
            ]));
            for line in record.output.lines() {
                tui_output::write_line(&Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::raw(line.to_string()),
                ]));
            }
            tui_output::write_blank();
        }
        None => {
            let total = renderer.tool_history.len();
            if total == 0 {
                dim_msg("No tool outputs recorded yet.".into());
            } else {
                warn_msg(format!(
                    "No tool output #{n}. Have {total} recorded (use /expand 1\u{2013}{total})."
                ));
            }
        }
    }
}
