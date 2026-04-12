//! Slash command handler for the TUI event loop.
//!
//! Dispatches `/command` input to the appropriate handler. Most commands
//! are handled via [`crate::repl::handle_command`] (shared with headless),
//! then translated into TUI-specific actions here.
//!
//! See [`crate`] module docs for the full command table.
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

use crate::repl::ReplAction;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use koda_core::persistence::Persistence;
use koda_core::trust;
// For /copy transcript export:
use crate::mouse_select::copy_to_clipboard;
use crate::transcript;

use koda_core::agent::KodaAgent;
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
    shared_mode: &trust::SharedTrustMode,
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
                    {
                        let prov = provider.read().await;
                        config.query_and_apply_capabilities(prov.as_ref()).await;
                    }
                    crate::tui_wizards::save_provider(config, &session.db).await;
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
                crate::tui_wizards::save_provider(config, &session.db).await;
                tui_output::ok_msg(buffer, format!("Model set to: {model}"));
            }
            SlashAction::Continue
        }
        ReplAction::PickModel => SlashAction::Continue,
        ReplAction::SetupProvider(_ptype, _base_url) => SlashAction::Continue,
        ReplAction::PickProvider => SlashAction::Continue,
        ReplAction::ShowHelp => {
            show_help(buffer);
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
        ReplAction::CopyResponse(n) => {
            handle_copy_response(buffer, session, n).await;
            SlashAction::Continue
        }
        ReplAction::Export(ref dest) => {
            handle_export(buffer, session, dest.as_deref()).await;
            SlashAction::Continue
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
    shared_mode: &trust::SharedTrustMode,
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
                            && let Some(m) = trust::TrustMode::parse(&mode_str)
                        {
                            trust::set_trust(shared_mode, m);
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

/// Print help output generated from the `SLASH_COMMANDS` registry.
///
/// Single source of truth: `completer::SLASH_COMMANDS` drives both
/// Tab completion and this help screen.
///
/// Sections:
/// 1. Commands — slash commands from the registry
/// 2. Input — text entry and file attachment
/// 3. Navigation — scroll and history search
/// 4. Session control — cancel, quit, clipboard
/// 5. Approval — keys shown when the engine requests tool approval
fn show_help(buffer: &mut ScrollBuffer) {
    use crate::completer::SLASH_COMMANDS;

    // Column width: longest command label (cmd + arg_hint) + padding
    let max_cmd_len = SLASH_COMMANDS
        .iter()
        .map(|(cmd, _, hint)| cmd.len() + hint.map(|h| h.len() + 1).unwrap_or(0))
        .max()
        .unwrap_or(10);
    let col = max_cmd_len.max(16) + 4; // at least 20 chars wide

    // ── Commands ───────────────────────────────────────────────
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Commands", BOLD));
    tui_output::blank(buffer);
    for &(cmd, desc, arg_hint) in SLASH_COMMANDS {
        let label = match arg_hint {
            Some(hint) => format!("{cmd} {hint}"),
            None => cmd.to_string(),
        };
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {label:<col$}"), CYAN),
                Span::styled(desc, DIM),
            ]),
        );
    }

    // ── Input ──────────────────────────────────────────────────
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Input", BOLD));
    tui_output::blank(buffer);
    let input_keys: &[(&str, &str)] = &[
        ("Enter", "Send message"),
        ("Alt+Enter", "Insert newline (multi-line input)"),
        ("@file.rs", "Attach file context to your message"),
        ("@image.png", "Attach image (vision-capable models)"),
        ("↑ / ↓", "Cycle through input history"),
        ("Ctrl+R", "Reverse history search"),
        ("Tab", "Autocomplete @file path or /command"),
        ("Shift+Tab", "Cycle approval mode (auto ↔ confirm)"),
    ];
    for (key, desc) in input_keys {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {key:<col$}"), CYAN),
                Span::styled(*desc, DIM),
            ]),
        );
    }

    // ── Navigation ─────────────────────────────────────────────
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Navigation", BOLD));
    tui_output::blank(buffer);
    let nav_keys: &[(&str, &str)] = &[
        ("PgUp / PgDn", "Scroll history up / down one page"),
        ("Home", "Jump to top of history"),
        ("End", "Jump to bottom (latest output)"),
        ("Mouse scroll", "Scroll history"),
    ];
    for (key, desc) in nav_keys {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {key:<col$}"), CYAN),
                Span::styled(*desc, DIM),
            ]),
        );
    }

    // ── Session control ────────────────────────────────────────
    tui_output::blank(buffer);
    tui_output::emit_line(buffer, Line::styled("  Session control", BOLD));
    tui_output::blank(buffer);
    let session_keys: &[(&str, &str)] = &[
        ("Esc / Ctrl+C", "Cancel running inference"),
        ("Ctrl+D", "Quit koda"),
    ];
    for (key, desc) in session_keys {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {key:<col$}"), CYAN),
                Span::styled(*desc, DIM),
            ]),
        );
    }

    // ── Approval (shown when a tool needs confirmation) ────────
    tui_output::blank(buffer);
    tui_output::emit_line(
        buffer,
        Line::from(vec![
            Span::styled("  Approval ", BOLD),
            Span::styled("(when the agent asks to run a tool)", DIM),
        ]),
    );
    tui_output::blank(buffer);
    let approval_keys: &[(&str, &str)] = &[
        ("y", "Approve this tool call"),
        ("n", "Reject this tool call"),
        ("a", "Approve and switch to auto mode (no more prompts)"),
        ("f", "Reject and provide written feedback"),
        ("Esc", "Reject (same as n)"),
    ];
    for (key, desc) in approval_keys {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled(format!("  {key:<col$}"), CYAN),
                Span::styled(*desc, DIM),
            ]),
        );
    }
    tui_output::blank(buffer);
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

// ── /copy ────────────────────────────────────────────────

/// Export the session transcript to clipboard or a Markdown file.
///
/// `/copy [n]` — copy the Nth-most-recent assistant response to clipboard.
///
/// Loads messages from the DB, filters to assistant turns with text content,
/// and copies the Nth-last one (1 = most recent).
async fn handle_copy_response(
    buffer: &mut ScrollBuffer,
    session: &koda_core::session::KodaSession,
    n: usize,
) {
    let messages = match session.db.load_all_messages(&session.id).await {
        Ok(msgs) => msgs,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not load messages: {e}"));
            return;
        }
    };

    // Collect assistant messages that have non-empty text content.
    let responses: Vec<&str> = messages
        .iter()
        .filter_map(|m| {
            use koda_core::persistence::Role;
            if m.role == Role::Assistant {
                m.content.as_deref().filter(|c| !c.trim().is_empty())
            } else {
                None
            }
        })
        .collect();

    let total = responses.len();
    if total == 0 {
        tui_output::dim_msg(buffer, "No assistant responses to copy.".into());
        return;
    }
    if n > total {
        tui_output::err_msg(
            buffer,
            format!(
                "Only {total} response{} in this session.",
                if total == 1 { "" } else { "s" }
            ),
        );
        return;
    }

    // responses is oldest-first; Nth-last = index (total - n)
    let content = responses[total - n];
    let label = if n == 1 {
        "last response".to_string()
    } else {
        format!("response #{n} from end")
    };

    match copy_to_clipboard(content) {
        Ok(msg) => {
            let preview: String = content.chars().take(60).collect();
            let preview = preview.replace('\n', " ");
            tui_output::ok_msg(buffer, format!("Copied {label} {msg} — {preview}\u{2026}"));
        }
        Err(e) => tui_output::err_msg(buffer, e),
    }
}

/// `/export [<file.md>]` — export the full session transcript.
///
/// - `dest = None`        → auto-name from first user prompt + timestamp, write to CWD
/// - `dest = Some(path)`  → write Markdown to that explicit path
///
/// Uses `transcript::render` to produce a structured Markdown document
/// from all session messages (system messages excluded).
async fn handle_export(
    buffer: &mut ScrollBuffer,
    session: &koda_core::session::KodaSession,
    dest: Option<&str>,
) {
    // Fetch all messages — `load_all_messages` includes compacted history.
    let messages = match session.db.load_all_messages(&session.id).await {
        Ok(msgs) => msgs,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not load messages: {e}"));
            return;
        }
    };

    // Try to get the session title for a nicer header.
    let title_storage;
    let session_title: Option<&str> = match session
        .db
        .list_sessions(200, std::path::Path::new("/"))
        .await
    {
        Ok(sessions) => {
            title_storage = sessions
                .into_iter()
                .find(|s| s.id == session.id)
                .and_then(|s| s.title);
            title_storage.as_deref()
        }
        Err(_) => None,
    };

    let md = transcript::render(&messages, session_title);

    let path_owned;
    let path: &str = match dest {
        Some(p) => {
            // Reject paths that escape CWD — absolute paths and ".." traversal.
            let dest_path = std::path::Path::new(p);
            if dest_path.is_absolute() || p.contains("..") {
                tui_output::err_msg(
                    buffer,
                    "Export path must be relative to the current directory \
                     (no absolute paths or \"..\" traversal)."
                        .into(),
                );
                return;
            }
            p
        }
        None => {
            path_owned = export_default_filename(&messages);
            &path_owned
        }
    };

    match std::fs::write(path, &md) {
        Ok(()) => {
            let lines = md.lines().count();
            tui_output::ok_msg(buffer, format!("Saved {lines} lines \u{2192} {path}"));
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not write {path}: {e}"));
        }
    }
}

/// Generate a default export filename from the first user message + UTC timestamp.
///
/// Format: `koda-YYYYMMDD-HHMMSS-<slug>.md`  (slug from first user prompt, max 40 chars).
/// Falls back to `koda-YYYYMMDD-HHMMSS.md` when no user message exists.
fn export_default_filename(messages: &[koda_core::persistence::Message]) -> String {
    let dt = crate::util::utc_now();
    let ts = format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    );

    let slug: String = messages
        .iter()
        .find(|m| {
            use koda_core::persistence::Role;
            m.role == Role::User
        })
        .and_then(|m| m.content.as_deref())
        .map(|c| {
            c.lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .filter(|c| c.is_alphanumeric() || c.is_whitespace())
                .take(50)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-")
                .to_lowercase()
        })
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Hard cap at 40 chars, respecting char boundaries.
            let truncated: String = s.chars().take(40).collect();
            truncated.trim_end_matches('-').to_string()
        })
        .unwrap_or_default();

    if slug.is_empty() {
        format!("koda-{ts}.md")
    } else {
        format!("koda-{ts}-{slug}.md")
    }
}

#[cfg(test)]
mod tests {
    use super::export_default_filename;
    use koda_core::persistence::{Message, Role};

    fn user_msg(content: &str) -> Message {
        Message {
            id: 0,
            session_id: "test".into(),
            role: Role::User,
            content: Some(content.to_string()),
            full_content: None,
            tool_calls: None,
            tool_call_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
        }
    }

    #[test]
    fn filename_format_no_messages() {
        let name = export_default_filename(&[]);
        // koda-YYYYMMDD-HHMMSS.md
        assert!(name.starts_with("koda-"), "got: {name}");
        assert!(name.ends_with(".md"), "got: {name}");
    }

    #[test]
    fn filename_includes_slug_from_first_user_msg() {
        let msgs = vec![user_msg("Refactor the auth module")];
        let name = export_default_filename(&msgs);
        assert!(name.contains("refactor"), "got: {name}");
        assert!(name.contains("auth"), "got: {name}");
        assert!(name.ends_with(".md"), "got: {name}");
    }

    #[test]
    fn filename_slug_is_lowercase_hyphenated() {
        let msgs = vec![user_msg("Fix Bug In Parser")];
        let name = export_default_filename(&msgs);
        assert!(name.contains("fix-bug-in-parser"), "got: {name}");
    }

    #[test]
    fn filename_slug_capped_at_40_chars() {
        let long = "a".repeat(200);
        let msgs = vec![user_msg(&long)];
        let name = export_default_filename(&msgs);
        // slug portion after the second "-" should be <= 40 chars
        let slug_part = name
            .trim_start_matches("koda-")
            .split('-')
            .skip(2) // skip YYYYMMDD and HHMMSS
            .collect::<Vec<_>>()
            .join("-");
        let slug_no_ext = slug_part.trim_end_matches(".md");
        assert!(
            slug_no_ext.len() <= 40,
            "slug too long ({} chars): {slug_no_ext}",
            slug_no_ext.len()
        );
    }
}
