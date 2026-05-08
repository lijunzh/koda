//! Slash command handler for the TUI event loop.
//!
//! Parses and dispatches `/command` input in a single pass — no intermediate
//! action enum. Each match arm parses its own argument(s) and calls the
//! appropriate handler directly.
//!
//! See [`crate`] module docs for the full command table.
//!
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`] render cache.

use crate::mouse_select::copy_to_clipboard;
use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;
use crate::tui_render::TuiRenderer;
use koda_core::persistence::Persistence;
use koda_core::trust;

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
    child_activity: &mut crate::child_activity::ChildActivityTracker,
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

        // #1210 deleted the legacy `/agents` interactive panel; live
        // bg-agent + bg-process state now flows through the always-on
        // `widgets::child_activity_overlay` rendered above the status bar.
        // The model-facing `ListBackgroundTasks` tool keeps the same
        // dump shape for in-context introspection — `/agents` was the
        // user-facing surface for the same data and is redundant.
        "/cancel" => {
            // Accepts: `agent:N`, `process:N`, or bare numeric (→ agent, back-compat).
            // Empty / whitespace-only / unparseable → None so the handler
            // can render a Usage: line instead of silently doing nothing.
            let parsed = arg.and_then(|s| {
                let s = s.trim();
                if s.is_empty() {
                    None
                } else {
                    koda_core::tools::task_id::parse_task_id(s).ok()
                }
            });
            crate::tui_bg_tasks::handle_cancel_background_task(
                buffer,
                &session.bg_agents,
                &agent.tools.bg_registry,
                child_activity,
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

        "/debug-bundle" => {
            handle_debug_bundle(buffer, session, config).await;
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
                        let mode_to_apply = match session
                            .db
                            .get_session_mode(&target.id)
                            .await
                            .ok()
                            .flatten()
                        {
                            Some(mode_str) => match trust::TrustMode::parse(&mode_str) {
                                Some(parsed) => {
                                    let (m, warning) = trust::coerce_for_top_level(parsed);
                                    let report = koda_core::sandbox::dependency_report();
                                    match trust::require_sandbox_for_auto(m, report.available) {
                                        Ok(validated) => Some((validated, warning)),
                                        Err(msg) => {
                                            tui_output::err_msg(
                                                buffer,
                                                format!(
                                                    "Cannot resume session in Auto mode: {msg}\n\nSetup:\n{}",
                                                    koda_core::sandbox::setup_hint(report.backend)
                                                ),
                                            );
                                            return;
                                        }
                                    }
                                }
                                None => None,
                            },
                            None => None,
                        };
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
                        // Restore persisted approval mode (#590).
                        // **#1244**: route the parsed mode through
                        // `coerce_for_top_level` so a session that
                        // somehow got persisted with `plan` (older
                        // koda version, manual DB edit, race) is
                        // coerced to Safe with a visible banner.
                        //
                        // **#1241 release audit**: validate Auto with
                        // `require_sandbox_for_auto` BEFORE switching
                        // `session.id`; otherwise a persisted Auto
                        // session could bypass the startup sandbox
                        // refusal when resumed on an unsandboxed host.
                        if let Some((m, warning)) = mode_to_apply {
                            if let Some(w) = warning {
                                tui_output::warn_msg(buffer, w.to_string());
                            }
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

/// `/debug-bundle` — write a self-contained `.zip` debug artifact to
/// `~/.config/koda/debug-bundles/`.
///
/// See [`crate::debug_bundle`] module docs for the bundle layout, design
/// rationale, and security policy. This handler's job is solely to
/// translate the live session into a [`debug_bundle::BundleInput`] and
/// surface success/failure to the TUI.
async fn handle_debug_bundle(
    buffer: &mut ScrollBuffer,
    session: &koda_core::session::KodaSession,
    config: &KodaConfig,
) {
    use crate::debug_bundle::{BundleInput, write_bundle};

    // Load the same data /debug-bundle uses — messages from DB, plus session info
    // for the metadata header.
    let messages = match session.db.load_all_messages(&session.id).await {
        Ok(m) => m,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not load messages: {e}"));
            return;
        }
    };

    // #1344 issue A: load persisted sub-agent events so the bundle's
    // `conversation.md` can fold them under each parent `InvokeAgent`
    // tool result. DB hiccup is non-fatal — ship a bundle without
    // sub-agent activity blocks rather than failing the whole export.
    let events = session
        .db
        .load_session_events(&session.id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, session_id = %session.id,
                "failed to load session_events for debug bundle; \
                 sub-agent activity blocks will be omitted");
            Vec::new()
        });

    let (session_title, session_started_at): (Option<String>, Option<String>) = match session
        .db
        .list_sessions(200, std::path::Path::new("/"))
        .await
    {
        Ok(sessions) => {
            let found = sessions.into_iter().find(|s| s.id == session.id);
            (
                found.as_ref().and_then(|s| s.title.clone()),
                found.map(|s| s.created_at),
            )
        }
        Err(_) => (None, None),
    };

    let config_dir = match koda_core::db::config_dir() {
        Ok(d) => d,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not resolve config dir: {e}"));
            return;
        }
    };
    let output_dir = config_dir.join("debug-bundles");

    let captured_at = chrono_like_now_iso();

    let input = BundleInput {
        session_id: &session.id,
        session_title: session_title.as_deref(),
        session_started_at: session_started_at.as_deref(),
        model: Some(config.model.as_str()),
        provider: Some(session.provider.provider_name()),
        context_window: None, // PR α: not surfacing yet; PR β can wire from provider
        messages: &messages,
        events: &events,
        config_dir: &config_dir,
        current_pid: std::process::id(),
        captured_at: &captured_at,
        output_dir: &output_dir,
    };

    match write_bundle(&input) {
        Ok(path) => {
            tui_output::ok_msg(
                buffer,
                format!("Wrote debug bundle \u{2192} {}", path.display()),
            );
            // Discoverability hint (RFC #1167 §D7 alternative): instead of
            // a dedicated /logs slash command, surface the raw log path
            // exactly when the user is thinking about debug artifacts.
            // The bundle README documents this too — this is the
            // belt-and-braces in-TUI surface.
            tui_output::dim_msg(
                buffer,
                format!("  raw logs: {}/logs/latest", config_dir.display()),
            );
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Could not write debug bundle: {e}"));
        }
    }
}

/// Best-effort RFC-3339 "now" using the shared [`crate::util::utc_now`]
/// helper (per #818, hand-rolled date math is banned in koda-cli).
///
/// Format: `YYYY-MM-DDTHH:MM:SSZ` (UTC). Used for `captured_at` in the
/// debug bundle metadata + bundle filename.
fn chrono_like_now_iso() -> String {
    let dt = crate::util::utc_now();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    )
}
