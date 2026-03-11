//! Slash command handler for the TUI event loop.
//!
//! All output rendered through `tui_output::write_line(&)` with native
//! ratatui `Line`/`Span` styling. Stays in raw mode.

use crate::repl::ReplAction;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use crate::tui_types::Term;
use koda_core::persistence::Persistence;

use koda_core::agent::KodaAgent;
use koda_core::approval;
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::RwLock;

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
            {
                let prov = provider.read().await;
                config.query_and_apply_capabilities(prov.as_ref()).await;
            }
            crate::tui_wizards::save_provider(config);
            ok_msg(format!("Model set to: {model}"));
            SlashAction::Continue
        }
        ReplAction::PickModel => {
            // Handled inline by tui_app.rs MenuContent::Model dropdown
            SlashAction::Continue
        }
        ReplAction::SetupProvider(_ptype, _base_url) => {
            // Handled inline by tui_app.rs ProviderWizard
            SlashAction::Continue
        }
        ReplAction::PickProvider => {
            // Handled inline by tui_app.rs MenuContent::Provider dropdown
            SlashAction::Continue
        }
        ReplAction::ShowHelp => {
            // /help is now handled by the auto-dropdown on /
            // Just show the tips line for backward compatibility
            tui_output::write_line(&Line::styled("  Type / to see available commands", DIM));
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
            // Handled inline by tui_app.rs MenuContent::Session dropdown
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

fn handle_expand(_terminal: &mut Term, renderer: &TuiRenderer, n: usize) {
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
