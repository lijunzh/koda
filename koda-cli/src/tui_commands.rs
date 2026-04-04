//! Slash command handler for the TUI event loop.
//!
//! All output flows through the `ScrollBuffer` render cache.

use crate::repl::ReplAction;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use koda_core::persistence::Persistence;

use koda_core::agent::KodaAgent;
use koda_core::approval;
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use koda_core::session::KodaSession;
use ratatui::text::{Line, Span};
use std::sync::Arc;
use tokio::sync::RwLock;

pub enum SlashAction {
    Continue,
    Quit,
    OpenKeyMenu,
}

use tui_output::{BOLD, CYAN, DIM};

// ── Main handler ────────────────────────────────────────

#[allow(clippy::too_many_arguments, unused_variables)]
pub async fn handle_slash_command(
    buffer: &mut ScrollBuffer,
    input: &str,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    session: &mut KodaSession,
    shared_mode: &approval::SharedMode,
    renderer: &mut TuiRenderer,
    project_root: &std::path::Path,
    agent: &Arc<KodaAgent>,
    pending_command: &mut Option<String>,
    menu: &mut crate::tui_types::MenuContent,
) -> SlashAction {
    match crate::repl::handle_command(input, config, provider).await {
        ReplAction::Quit => SlashAction::Quit,
        ReplAction::SwitchModel(model) => {
            // Check if it's an alias first
            if let Some(resolved) = koda_core::model_alias::resolve(&model) {
                let ptype = resolved.provider;
                if ptype.requires_api_key() && !koda_core::runtime_env::is_set(ptype.env_key_name())
                {
                    tui_output::err_msg(
                        buffer,
                        format!("{} not set. Run /key to configure.", ptype.env_key_name()),
                    );
                } else if resolved.needs_auto_detect() {
                    tui_output::warn_msg(
                        buffer,
                        "Use /model to pick local models interactively.".to_string(),
                    );
                } else {
                    config.provider_type = ptype;
                    config.base_url = ptype.default_base_url().to_string();
                    config.model = resolved.model_id.to_string();
                    config.model_settings.model = config.model.clone();
                    config.recalculate_model_derived();
                    *provider.write().await = koda_core::providers::create_provider(config);
                    crate::tui_wizards::save_provider(config);
                    tui_output::ok_msg(
                        buffer,
                        format!(
                            "Model: {} ({}, {})",
                            resolved.alias, resolved.model_id, ptype
                        ),
                    );
                }
            } else {
                // Literal model ID — switch on current provider
                config.model = model.clone();
                config.model_settings.model = model.clone();
                config.recalculate_model_derived();
                {
                    let prov = provider.read().await;
                    config.query_and_apply_capabilities(prov.as_ref()).await;
                }
                crate::tui_wizards::save_provider(config);
                tui_output::ok_msg(buffer, format!("Model set to: {model}"));
            }
            SlashAction::Continue
        }
        ReplAction::PickModel => SlashAction::Continue,
        ReplAction::SetupProvider(_ptype, _base_url) => SlashAction::Continue,
        ReplAction::PickProvider => SlashAction::Continue,
        ReplAction::ShowHelp => {
            tui_output::dim_msg(buffer, "Type / to see available commands".into());
            SlashAction::Continue
        }
        ReplAction::Undo => {
            match agent.tools.undo.lock() {
                Ok(mut undo) => match undo.undo() {
                    Some(summary) => tui_output::ok_msg(buffer, summary),
                    None => tui_output::warn_msg(buffer, "Nothing to undo.".to_string()),
                },
                Err(e) => tui_output::err_msg(buffer, format!("Undo error: {e}")),
            }
            SlashAction::Continue
        }
        ReplAction::ListSessions => SlashAction::Continue,
        ReplAction::DeleteSession(ref id) => {
            handle_delete_session(buffer, session, id, project_root).await;
            SlashAction::Continue
        }
        ReplAction::ResumeSession(ref id) => {
            handle_resume_session(buffer, session, id, project_root, shared_mode).await;
            SlashAction::Continue
        }
        ReplAction::InjectPrompt(prompt) => {
            *pending_command = Some(prompt);
            SlashAction::Continue
        }
        ReplAction::Compact => {
            crate::tui_wizards::handle_compact(buffer, session, config, provider).await;
            SlashAction::Continue
        }
        ReplAction::Purge(ref age_filter) => {
            crate::tui_wizards::handle_purge(buffer, session, age_filter.as_deref(), menu).await;
            SlashAction::Continue
        }
        ReplAction::Expand(n) => {
            handle_expand(buffer, renderer, n);
            SlashAction::Continue
        }
        ReplAction::Verbose(v) => {
            renderer.verbose = match v {
                Some(val) => val,
                None => !renderer.verbose,
            };
            let state = if renderer.verbose { "on" } else { "off" };
            tui_output::emit_line(
                buffer,
                Line::styled(format!("  Verbose tool output: {state}"), CYAN),
            );
            SlashAction::Continue
        }
        ReplAction::ListAgents => {
            crate::tui_wizards::handle_list_agents(buffer, project_root);
            SlashAction::Continue
        }
        ReplAction::ShowDiff => {
            crate::tui_wizards::handle_diff(buffer);
            SlashAction::Continue
        }
        ReplAction::MemoryCommand(ref arg) => {
            crate::tui_wizards::handle_memory(buffer, arg.as_deref(), project_root);
            SlashAction::Continue
        }
        ReplAction::ListSkills(ref query) => {
            crate::tui_wizards::handle_list_skills(buffer, query.as_deref(), &agent.tools);
            SlashAction::Continue
        }
        ReplAction::ManageKeys => {
            crate::tui_wizards::handle_keys(buffer);
            SlashAction::OpenKeyMenu
        }
        ReplAction::Handled => SlashAction::Continue,
        ReplAction::NotACommand => SlashAction::Continue,
    }
}

// ── Sub-handlers ────────────────────────────────────────

async fn handle_delete_session(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    id: &str,
    project_root: &std::path::Path,
) {
    if id == session.id {
        tui_output::err_msg(buffer, "Cannot delete the current session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => tui_output::err_msg(buffer, format!("No session found matching '{id}'.")),
                    1 => {
                        let full_id = &matches[0].id;
                        match session.db.delete_session(full_id).await {
                            Ok(true) => tui_output::ok_msg(
                                buffer,
                                format!("Deleted session {}", &full_id[..8]),
                            ),
                            Ok(false) => tui_output::err_msg(buffer, "Session not found.".into()),
                            Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
                        }
                    }
                    n => tui_output::err_msg(
                        buffer,
                        format!("Ambiguous: '{id}' matches {n} sessions. Be more specific."),
                    ),
                }
            }
            Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
        }
    }
}

async fn handle_resume_session(
    buffer: &mut ScrollBuffer,
    session: &mut KodaSession,
    id: &str,
    project_root: &std::path::Path,
    shared_mode: &approval::SharedMode,
) {
    use tui_output::GREEN;
    if session.id.starts_with(id) {
        tui_output::dim_msg(buffer, "Already in this session.".into());
    } else {
        match session.db.list_sessions(100, project_root).await {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => tui_output::err_msg(buffer, format!("No session found matching '{id}'.")),
                    1 => {
                        let target = &matches[0];
                        session.id = target.id.clone();
                        session.title_set = true;
                        let short_id = target.id[..8].to_string();
                        let title_part = target
                            .title
                            .as_deref()
                            .map(|t| format!(" — {}", t.chars().take(40).collect::<String>()))
                            .unwrap_or_default();
                        let detail = format!(
                            "{title_part}  {}  {} msgs",
                            target.created_at, target.message_count
                        );
                        tui_output::emit_line(
                            buffer,
                            Line::from(vec![
                                Span::styled("  \u{2713} ", GREEN),
                                Span::raw("Resumed session "),
                                Span::styled(short_id, CYAN),
                                Span::styled(detail, DIM),
                            ]),
                        );
                        // Restore persisted approval mode (#590)
                        if let Ok(Some(mode_str)) = session.db.get_session_mode(&session.id).await
                            && let Some(m) = approval::ApprovalMode::parse(&mode_str)
                        {
                            approval::set_mode(shared_mode, m);
                        }
                        // Read idle time BEFORE load_context updates last_accessed_at.
                        let idle_secs = session
                            .db
                            .get_session_idle_secs(&session.id)
                            .await
                            .ok()
                            .flatten();
                        // Show away-summary + interrupted-turn banners (#590, #594)
                        if let Ok(msgs) = session.db.load_context(&session.id).await {
                            use koda_core::persistence::Role;
                            let user_msgs = msgs.iter().filter(|m| m.role == Role::User).count();
                            let tool_calls = msgs.iter().filter(|m| m.role == Role::Tool).count();
                            let total_tokens: i64 = msgs
                                .iter()
                                .map(|m| {
                                    m.prompt_tokens.unwrap_or(0) + m.completion_tokens.unwrap_or(0)
                                })
                                .sum();
                            buffer.push_lines(tui_output::away_summary_banner(
                                idle_secs,
                                None, // title already in the confirmation line above
                                user_msgs,
                                tool_calls,
                                total_tokens,
                            ));
                            if let Some(kind) = koda_core::db::queries::detect_interruption(&msgs) {
                                buffer.push_lines(tui_output::interrupted_turn_banner(&kind));
                            }
                        }
                    }
                    n => tui_output::err_msg(
                        buffer,
                        format!("Ambiguous: '{id}' matches {n} sessions. Be more specific."),
                    ),
                }
            }
            Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
        }
    }
}

fn handle_expand(buffer: &mut ScrollBuffer, renderer: &TuiRenderer, n: usize) {
    match renderer.tool_history.get(n) {
        Some(record) => {
            tui_output::blank(buffer);
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled(format!("  \u{1f50d} Expand: {}", record.tool_name), BOLD),
                    Span::styled(format!(" ({} lines)", record.output.lines().count()), DIM),
                ]),
            );
            for line in record.output.lines() {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::styled("  \u{2502} ", DIM),
                        Span::raw(line.to_string()),
                    ]),
                );
            }
            tui_output::blank(buffer);
        }
        None => {
            let total = renderer.tool_history.len();
            if total == 0 {
                tui_output::dim_msg(buffer, "No tool outputs recorded yet.".into());
            } else {
                tui_output::warn_msg(
                    buffer,
                    format!(
                        "No tool output #{n}. Have {total} recorded (use /expand 1\u{2013}{total})."
                    ),
                );
            }
        }
    }
}
