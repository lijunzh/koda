//! Slash command handler for the TUI event loop.
//!
//! Parses and dispatches `/command` input in a single pass — no intermediate
//! action enum. Each match arm parses its own argument(s) and calls the
//! appropriate handler directly.
//!
//! See [`crate`] module docs for the full command table.
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

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
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match cmd {
        "/exit" => SlashAction::Quit,

        "/model" => match arg {
            Some(model) => {
                if let Some(resolved) = koda_core::model_alias::resolve(model) {
                    let ptype = resolved.provider;
                    if ptype.requires_api_key()
                        && !koda_core::runtime_env::is_set(ptype.env_key_name())
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
                    // Literal model ID — switch on current provider.
                    config.model = model.to_string();
                    config.model_settings.model = model.to_string();
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
            None => SlashAction::Continue, // opens model picker via TUI context
        },

        // Provider picker is opened by the TUI context; /provider with or
        // without an arg just signals Continue and lets the app layer handle it.
        "/provider" => SlashAction::Continue,

        "/help" => {
            show_help(buffer);
            SlashAction::Continue
        }

        "/diff" => {
            match arg {
                Some("review") => {
                    let diff = get_git_diff();
                    *pending_command = Some(format!(
                        "Review these uncommitted changes. Point out bugs, \
                         improvements, and concerns:\n\n```diff\n{diff}\n```"
                    ));
                }
                Some("commit") => {
                    let diff = get_git_diff();
                    *pending_command = Some(format!(
                        "Write a conventional commit message for these changes. \
                         Use the format: type: description\n\nInclude a body with \
                         bullet points for each logical change.\n\n```diff\n{diff}\n```"
                    ));
                }
                _ => crate::tui_wizards::handle_diff(buffer),
            }
            SlashAction::Continue
        }

        "/compact" => {
            crate::tui_wizards::handle_compact(buffer, session, config, provider).await;
            SlashAction::Continue
        }

        "/purge" => {
            crate::tui_wizards::handle_purge(buffer, session, arg, menu).await;
            SlashAction::Continue
        }

        "/expand" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1);
            handle_expand(buffer, renderer, n);
            SlashAction::Continue
        }

        "/verbose" => {
            renderer.verbose = match arg {
                Some("on") => true,
                Some("off") => false,
                _ => !renderer.verbose,
            };
            let state = if renderer.verbose { "on" } else { "off" };
            tui_output::emit_line(
                buffer,
                Line::styled(format!("  Verbose tool output: {state}"), CYAN),
            );
            SlashAction::Continue
        }

        "/agent" => {
            crate::tui_wizards::handle_list_agents(buffer, project_root);
            SlashAction::Continue
        }

        // #996 Layers 1+F — unified runtime task surface. `/agents` (plural)
        // lists running tasks; `/agent` (singular) lists sub-agent definitions.
        "/agents" => {
            crate::tui_bg_tasks::handle_list_background_tasks(
                buffer,
                &session.bg_agents,
                &agent.tools.bg_registry,
            );
            SlashAction::Continue
        }

        "/cancel" => {
            // Accepts: `agent:N`, `process:N`, or bare numeric (→ agent, back-compat).
            // Empty / whitespace-only / unparseable → None so the handler
            // can render a Usage: line instead of silently doing nothing.
            let parsed = arg.and_then(|s| {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    koda_core::tools::bg_task_tools::parse_task_id(s).ok()
                }
            });
            crate::tui_bg_tasks::handle_cancel_background_task(
                buffer,
                &session.bg_agents,
                &agent.tools.bg_registry,
                parsed,
            );
            SlashAction::Continue
        }

        "/sessions" => {
            match arg {
                Some(sub) if sub.starts_with("delete ") => {
                    let id = sub.strip_prefix("delete ").unwrap().trim().to_string();
                    handle_delete_session(buffer, session, &id, project_root).await;
                }
                Some(sub) if sub.starts_with("resume ") => {
                    let id = sub.strip_prefix("resume ").unwrap().trim().to_string();
                    handle_resume_session(buffer, session, &id, project_root, shared_mode).await;
                }
                // Bare ID shorthand: /sessions <hex-id>
                Some(id)
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
                {
                    handle_resume_session(buffer, session, id, project_root, shared_mode).await;
                }
                _ => {} // no-op: session list is opened by the TUI context
            }
            SlashAction::Continue
        }

        "/memory" => {
            crate::tui_wizards::handle_memory(buffer, arg, project_root);
            SlashAction::Continue
        }

        "/undo" => {
            match agent.tools.undo.lock() {
                Ok(mut undo) => match undo.undo() {
                    Some(summary) => tui_output::ok_msg(buffer, summary),
                    None => tui_output::warn_msg(buffer, "Nothing to undo.".to_string()),
                },
                Err(e) => tui_output::err_msg(buffer, format!("Undo error: {e}")),
            }
            SlashAction::Continue
        }

        "/skills" => {
            crate::tui_wizards::handle_list_skills(buffer, arg, &agent.tools);
            SlashAction::Continue
        }

        "/key" | "/keys" => {
            crate::tui_wizards::handle_keys(buffer);
            SlashAction::OpenKeyMenu
        }

        "/copy" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
            handle_copy_response(buffer, session, n).await;
            SlashAction::Continue
        }

        "/export" => {
            let (summary, dest) = match arg {
                Some(s) => {
                    let parts: Vec<&str> = s.splitn(2, ' ').collect();
                    if parts.first() == Some(&"--summary") {
                        (true, parts.get(1).map(|d| d.to_string()))
                    } else {
                        (false, Some(s.to_string()))
                    }
                }
                None => (false, None),
            };
            handle_export(
                buffer,
                session,
                dest.as_deref(),
                summary,
                config,
                project_root,
            )
            .await;
            SlashAction::Continue
        }

        "/mcp" => {
            dispatch_mcp(buffer, session, agent, arg).await;
            SlashAction::Continue
        }

        _ => SlashAction::Continue,
    }
}

// ── MCP dispatch ────────────────────────────────────────

async fn dispatch_mcp(
    buffer: &mut ScrollBuffer,
    session: &koda_core::session::KodaSession,
    agent: &Arc<koda_core::agent::KodaAgent>,
    arg: Option<&str>,
) {
    let arg = match arg.filter(|s| !s.is_empty()) {
        Some(a) => a,
        None => {
            crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
            return;
        }
    };

    let mut tokens = arg.split_whitespace();
    let sub = tokens.next().unwrap_or("");

    match sub {
        "list" | "status" => {
            crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
        }
        "add" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => {
                    crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
                    return;
                }
            };
            let command = match tokens.next() {
                Some(c) => c.to_string(),
                None => {
                    crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
                    return;
                }
            };
            let args: Vec<String> = tokens.map(String::from).collect();
            crate::tui_mcp::handle_mcp_add(buffer, session, agent, name, command, args).await;
        }
        "add-http" | "add_http" => {
            let name = match tokens.next() {
                Some(n) => n.to_string(),
                None => {
                    crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
                    return;
                }
            };
            let url = match tokens.next() {
                Some(u) => u.to_string(),
                None => {
                    crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
                    return;
                }
            };
            let bearer_token = parse_optional_flag(&mut tokens, "--token");
            crate::tui_mcp::handle_mcp_add_http(buffer, session, agent, name, url, bearer_token)
                .await;
        }
        "remove" | "rm" | "delete" => match tokens.next() {
            Some(n) => {
                crate::tui_mcp::handle_mcp_remove(buffer, session, agent, n.to_string()).await;
            }
            None => {
                crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
            }
        },
        "reconnect" | "retry" | "restart" => match tokens.next() {
            Some(n) => {
                crate::tui_mcp::handle_mcp_reconnect(buffer, agent, n.to_string()).await;
            }
            None => {
                crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
            }
        },
        _ => {
            crate::tui_mcp::handle_mcp_list(buffer, session, agent).await;
        }
    }
}

/// Parse `--flag <value>` from remaining tokens. Consumes both if found.
fn parse_optional_flag(tokens: &mut std::str::SplitWhitespace<'_>, flag: &str) -> Option<String> {
    let remaining: Vec<&str> = tokens.collect();
    let mut i = 0;
    while i < remaining.len() {
        if remaining[i] == flag && i + 1 < remaining.len() {
            return Some(remaining[i + 1].to_string());
        }
        i += 1;
    }
    None
}

/// Get the full git diff (unstaged + staged), capped for context window safety.
fn get_git_diff() -> String {
    const MAX_DIFF_CHARS: usize = 30_000;

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };

    let unstaged = run(&["diff"]);
    let staged = run(&["diff", "--cached"]);

    let mut diff = unstaged;
    if !staged.is_empty() {
        if !diff.is_empty() {
            diff.push_str("\n# --- Staged changes ---\n\n");
        }
        diff.push_str(&staged);
    }

    if diff.len() > MAX_DIFF_CHARS {
        let mut end = MAX_DIFF_CHARS;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n\n[TRUNCATED: diff was {} chars, showing first {}]",
            &diff[..end],
            diff.len(),
            MAX_DIFF_CHARS,
        )
    } else {
        diff
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
        (
            "drag & drop",
            "Drop an image into the terminal to attach it",
        ),
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

/// `/export [--summary] [<file.md>]` — export the session transcript.
///
/// Default: verbose (full fidelity). `--summary`: concise, human-readable.
///
/// Uses `transcript::render` to produce a structured Markdown document
/// from all session messages (system messages excluded).
async fn handle_export(
    buffer: &mut ScrollBuffer,
    session: &koda_core::session::KodaSession,
    dest: Option<&str>,
    summary: bool,
    config: &KodaConfig,
    project_root: &std::path::Path,
) {
    // Fetch all messages — `load_all_messages` includes compacted history.
    let messages = match session.db.load_all_messages(&session.id).await {
        Ok(msgs) => msgs,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not load messages: {e}"));
            return;
        }
    };

    // #1108 P1b/P2a: load engine events captured during the session
    // (top-level info / bg-task transitions, plus folded sub-agent
    // traces). Treat any DB error as "no events" and continue — the
    // transcript without events is still useful, and an empty slice
    // is a no-op for the renderer.
    let events = session
        .db
        .load_session_events(&session.id)
        .await
        .unwrap_or_default();

    // Try to get the session title and start time for the header.
    let title_storage;
    let started_storage;
    let (session_title, started_at): (Option<&str>, Option<&str>) = match session
        .db
        .list_sessions(200, std::path::Path::new("/"))
        .await
    {
        Ok(sessions) => {
            let found = sessions.into_iter().find(|s| s.id == session.id);
            title_storage = found.as_ref().and_then(|s| s.title.clone());
            started_storage = found.map(|s| s.created_at);
            (title_storage.as_deref(), started_storage.as_deref())
        }
        Err(_) => (None, None),
    };

    let meta = transcript::SessionMeta {
        session_id: session.id.clone(),
        title: session_title.map(|s| s.to_string()),
        started_at: started_at.map(|s| s.to_string()),
        model: config.model.clone(),
        provider: session.provider.provider_name().to_string(),
        project_root: project_root.display().to_string(),
    };

    let verbose = !summary;
    let md = transcript::render(&messages, &events, &meta, verbose);

    let path_owned;
    let path: &str = match dest {
        Some(p) => match validate_export_dest(p) {
            Ok(()) => p,
            Err(msg) => {
                tui_output::err_msg(buffer, msg.into());
                return;
            }
        },
        None => {
            path_owned = export_default_filename(&messages);
            &path_owned
        }
    };

    match write_export_file(std::path::Path::new(path), &md) {
        Ok(lines) => {
            tui_output::ok_msg(buffer, format!("Saved {lines} lines \u{2192} {path}"));
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not write {path}: {e}"));
        }
    }
}

/// Validate a user-supplied `/export <path>` destination.
///
/// Policy: relative paths only, no `..` traversal. Paths failing either
/// rule return an error message suitable for direct display in the TUI.
///
/// Note: the `..` check is a substring match (current historical behavior),
/// so filenames like `"v1..v2.md"` are also rejected. A future change could
/// switch to `Component::ParentDir` for a precise check; the existing tests
/// pin the substring behavior so any such change must update them.
fn validate_export_dest(dest: &str) -> Result<(), &'static str> {
    let dest_path = std::path::Path::new(dest);
    if dest_path.is_absolute() || dest.contains("..") {
        return Err("Export path must be relative to the current directory \
             (no absolute paths or \"..\" traversal).");
    }
    Ok(())
}

/// Write a transcript export to disk and return the line count.
///
/// Thin wrapper around `std::fs::write` that exposes the line-count
/// computation used by the TUI status message, and provides a single
/// well-tested seam for the file-creation path of `/export`.
fn write_export_file(path: &std::path::Path, content: &str) -> std::io::Result<usize> {
    std::fs::write(path, content)?;
    Ok(content.lines().count())
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
    use super::{export_default_filename, validate_export_dest, write_export_file};
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

    // ── validate_export_dest (#895) ───────────────────────────────────────

    #[test]
    fn validate_export_dest_accepts_simple_filename() {
        assert!(validate_export_dest("transcript.md").is_ok());
        assert!(validate_export_dest("koda-export.md").is_ok());
    }

    #[test]
    fn validate_export_dest_accepts_relative_subdir_path() {
        assert!(validate_export_dest("exports/today.md").is_ok());
        assert!(validate_export_dest("./output.md").is_ok());
    }

    #[test]
    fn validate_export_dest_rejects_absolute_path() {
        let err = validate_export_dest("/tmp/leak.md").unwrap_err();
        assert!(
            err.contains("relative"),
            "error must mention 'relative': {err}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn validate_export_dest_rejects_windows_drive_path() {
        // C:\foo\bar.md is absolute on Windows.
        assert!(validate_export_dest(r"C:\foo\bar.md").is_err());
    }

    #[test]
    fn validate_export_dest_rejects_parent_traversal() {
        for p in ["../escape.md", "foo/../bar.md", "../../../etc/passwd"] {
            let err = validate_export_dest(p).unwrap_err();
            assert!(
                err.contains("\"..\"") || err.contains("traversal"),
                "error for {p:?} must mention traversal: {err}"
            );
        }
    }

    /// Documents the current (overly broad) substring check: anything
    /// containing `..` is rejected, even when it's not a path component.
    /// If we ever switch to `Component::ParentDir` this test must flip.
    #[test]
    fn validate_export_dest_rejects_any_double_dot_substring() {
        // Not a real traversal — just a filename with two dots in a row.
        assert!(
            validate_export_dest("v1..v2.md").is_err(),
            "current behavior: substring check is intentionally strict"
        );
    }

    // ── write_export_file (#895) ──────────────────────────────────────────

    #[test]
    fn write_export_file_creates_file_with_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.md");
        let content = "# Header\n\nbody line\nfinal line\n";

        let lines = write_export_file(&path, content).expect("write");

        assert!(path.exists(), "file must exist on disk");
        let read_back = std::fs::read_to_string(&path).expect("read");
        assert_eq!(read_back, content, "file contents must match input");
        assert_eq!(lines, 4, "line count must match content.lines() count");
    }

    #[test]
    fn write_export_file_returns_zero_for_empty_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.md");

        let lines = write_export_file(&path, "").expect("write empty");
        assert_eq!(lines, 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn write_export_file_overwrites_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clobber.md");
        std::fs::write(&path, "OLD CONTENT THAT MUST BE GONE").unwrap();

        let new_content = "new line one\nnew line two\n";
        let lines = write_export_file(&path, new_content).expect("overwrite");

        assert_eq!(lines, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), new_content);
    }

    #[test]
    fn write_export_file_fails_when_parent_dir_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope/does-not-exist/out.md");

        let err =
            write_export_file(&path, "x").expect_err("writing to nonexistent parent must fail");
        // io::Error kind for this is NotFound on all platforms.
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ── End-to-end: render → write → verify file (#895) ───────────────────────
    //
    // The transcript module already covers verbose-vs-summary content
    // differences; these tests just close the loop by piping the rendered
    // markdown through write_export_file and reading it back. Mirrors the
    // exact data flow of handle_export() minus the DB load.

    use crate::transcript;

    fn meta_for_test() -> transcript::SessionMeta {
        transcript::SessionMeta {
            session_id: "sess-895".into(),
            title: Some("Export Coverage".into()),
            started_at: Some("2026-04-17T19:00:00Z".into()),
            model: "claude-sonnet-4".into(),
            provider: "anthropic".into(),
            project_root: "/tmp/proj".into(),
        }
    }

    fn assistant_with_tool_call(text: &str, tool_call_id: &str, tool_name: &str) -> Message {
        let mut msg = user_msg(text);
        msg.role = Role::Assistant;
        msg.tool_calls = Some(
            serde_json::json!([{
                "id": tool_call_id,
                "function": { "name": tool_name, "arguments": r#"{"file_path":"src/main.rs"}"# }
            }])
            .to_string(),
        );
        msg
    }

    fn tool_result(call_id: &str, content: &str) -> Message {
        let mut m = user_msg(content);
        m.role = Role::Tool;
        m.tool_call_id = Some(call_id.into());
        m
    }

    #[test]
    fn end_to_end_verbose_export_includes_tool_output_in_file() {
        let msgs = vec![
            user_msg("please read main.rs"),
            assistant_with_tool_call("", "call_v", "Read"),
            tool_result("call_v", "fn main() { println!(\"hi\"); }"),
        ];
        let md = transcript::render(&msgs, &[], &meta_for_test(), /*verbose=*/ true);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("verbose.md");
        let lines = write_export_file(&path, &md).expect("write");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("fn main()"),
            "verbose export file must contain Read tool output"
        );
        assert!(
            on_disk.contains("sess-895"),
            "verbose export must include session metadata header"
        );
        assert_eq!(
            lines,
            md.lines().count(),
            "reported line count matches rendered md"
        );
    }

    #[test]
    fn end_to_end_summary_export_omits_bash_output_from_file() {
        // Bash output is a *mutating* tool result — summary mode shows a
        // line-count placeholder rather than the raw output.
        let msgs = vec![
            user_msg("list files please"),
            {
                let mut m = assistant_with_tool_call("", "call_s", "Bash");
                m.tool_calls = Some(
                    serde_json::json!([{
                        "id": "call_s",
                        "function": { "name": "Bash", "arguments": r#"{"command":"ls"}"# }
                    }])
                    .to_string(),
                );
                m
            },
            tool_result("call_s", "alpha.txt\nbeta.txt\ngamma.txt"),
        ];
        let md = transcript::render(&msgs, &[], &meta_for_test(), /*verbose=*/ false);

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("summary.md");
        write_export_file(&path, &md).expect("write");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            !on_disk.contains("alpha.txt"),
            "summary export must NOT include raw Bash output. got:\n{on_disk}"
        );
        assert!(
            on_disk.contains("line(s) of output"),
            "summary export must include the placeholder summary. got:\n{on_disk}"
        );
    }
}
