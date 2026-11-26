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
use koda_client::protocol::state::{SandboxState, TrustMode};

use crate::tui_context::TuiSession;
use ratatui::text::{Line, Span};

pub enum SlashAction {
    Continue,
    Quit,
    OpenKeyMenu,
    /// `/feedback`: open its picker.
    OpenFeedback,
    /// `/model <name>`: the host changes the session's model.
    ChangeModel(String),
    /// Compact the session on the host, as a turn the TUI follows.
    Compact,
    /// `/memory`, with what follows it: the host reads and saves the notes.
    Memory(Option<String>),
}

use tui_output::{BOLD, CYAN, DIM};

// ── Main handler ────────────────────────────────────────

#[allow(clippy::too_many_arguments, unused_variables)]
pub async fn handle_slash_command(
    buffer: &mut ScrollBuffer,
    input: &str,
    session: &mut TuiSession,
    mode: (&mut TrustMode, &SandboxState),
    renderer: &mut TuiRenderer,
    project_root: &std::path::Path,
    pending_command: &mut Option<String>,
    menu: &mut crate::tui_types::MenuContent,
    child_activity: &mut crate::child_activity::ChildActivityTracker,
    client: &koda_client::InProcessClient,
) -> SlashAction {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts.get(1).map(|s| s.trim());

    match cmd {
        "/exit" | "/quit" => SlashAction::Quit,

        // The host changes the model, within the session's provider.
        "/model" => match arg {
            Some(model) => SlashAction::ChangeModel(model.to_string()),
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

        "/compact" => SlashAction::Compact,

        "/expand" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1);
            handle_expand(buffer, renderer, n);
            SlashAction::Continue
        }

        "/verbose" => {
            // The same switch as Ctrl+O: tool output in full, or folded.
            let full = match arg {
                Some("on") => true,
                Some("off") => false,
                _ => !buffer.is_full(),
            };
            buffer.set_full(full);
            SlashAction::Continue
        }

        "/agents" | "/agent" => {
            if cmd == "/agent" {
                tui_output::dim_msg(buffer, "/agent is now /agents.".into());
            }
            crate::tui_context::background::show_agents(
                &client.request_handle(),
                &session.id,
                buffer,
            )
            .await;
            SlashAction::Continue
        }

        // Codex's `/stop`, with its `/clean` alias.
        "/stop" | "/clean" => {
            crate::tui_context::background::stop_background(
                &client.request_handle(),
                &session.id,
                buffer,
                child_activity,
            )
            .await;
            SlashAction::Continue
        }

        "/resume" | "/sessions" => {
            if cmd == "/sessions" {
                tui_output::dim_msg(buffer, "/sessions is now /resume.".into());
            }
            match arg {
                Some(sub) if sub.starts_with("delete ") => {
                    let id = sub.strip_prefix("delete ").unwrap().trim().to_string();
                    handle_delete_session(buffer, session, client, &id).await;
                }
                Some(sub) if sub.starts_with("resume ") => {
                    let id = sub.strip_prefix("resume ").unwrap().trim().to_string();
                    handle_resume_session(buffer, session, client, &id, mode, renderer).await;
                }
                // Bare ID shorthand: /resume <hex-id>
                Some(id)
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
                {
                    handle_resume_session(buffer, session, client, id, mode, renderer).await;
                }
                _ => {} // no-op: session list is opened by the TUI context
            }
            SlashAction::Continue
        }

        "/memory" => SlashAction::Memory(arg.map(str::to_string)),

        "/skills" => {
            crate::tui_context::skills::show_skills(
                &client.request_handle(),
                client.project_root(),
                arg,
                buffer,
            )
            .await;
            SlashAction::Continue
        }

        "/key" | "/keys" => {
            // The key picker prints the providers' key table first.
            SlashAction::OpenKeyMenu
        }

        "/copy" => {
            let n: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
            handle_copy_response(buffer, renderer.replies(), n);
            SlashAction::Continue
        }

        "/feedback" => SlashAction::OpenFeedback,

        _ => {
            tui_output::warn_msg(buffer, format!("Unknown command: {cmd}. Try /help."));
            SlashAction::Continue
        }
    }
}

/// Get the full git diff (unstaged + staged), capped for context window safety.
fn get_git_diff() -> String {
    const MAX_DIFF_CHARS: usize = 30_000;

    // Hardened by `koda_git`: the repository's config can't make it run a
    // program (an external diff, a textconv, a filter, the fsmonitor).
    let run = |operation: koda_git::Operation, args: &[&str]| {
        koda_git::command(std::path::Path::new("."), operation)
            .ok()?
            .args(args)
            .args(koda_git::DIFF_ARGS)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
    };

    let unstaged = run(koda_git::Operation::WorkingTree, &["diff"]).unwrap_or_default();
    let staged = run(koda_git::Operation::Metadata, &["diff", "--cached"]).unwrap_or_default();

    let mut diff = unstaged;
    if !staged.is_empty() {
        if !diff.is_empty() {
            diff.push_str("\n# --- Staged changes ---\n\n");
        }
        diff.push_str(&staged);
    }
    // New files, each as a diff that adds it, as Codex's `/diff` shows them.
    // The cap below bounds how much of them goes in.
    let here = std::path::Path::new(".");
    for file in koda_git::untracked_files(here) {
        if diff.len() > MAX_DIFF_CHARS {
            break;
        }
        if let Some(added) = koda_git::untracked_diff(here, &file) {
            diff.push_str(&added);
        }
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

/// The project's stored sessions, newest first, from the host.
pub(crate) async fn list_sessions(
    client: &koda_client::InProcessClient,
    limit: u32,
) -> Result<Vec<koda_client::protocol::session::Session>, String> {
    use koda_client::protocol::{methods, session::SessionListParams};
    client
        .request::<methods::SessionList>(SessionListParams {
            cwd: Some(koda_client::protocol::session::SessionListCwd::One(
                client.project_root().display().to_string(),
            )),
            limit: Some(limit),
            ..Default::default()
        })
        .await
        .map(|listed| listed.data)
        .map_err(|e| e.to_string())
}

async fn handle_delete_session(
    buffer: &mut ScrollBuffer,
    session: &TuiSession,
    client: &koda_client::InProcessClient,
    id: &str,
) {
    use koda_client::protocol::{methods, session::SessionDeleteParams};
    let sessions = match crate::session_id::all_sessions(client)
        .await
        .map_err(|e| e.to_string())
    {
        Ok(sessions) => sessions,
        Err(e) => return tui_output::err_msg(buffer, format!("Error: {e}")),
    };
    let known: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let full_id = match session_to_delete(id, &known, &session.id) {
        Ok(full_id) => full_id,
        Err(why) => return tui_output::err_msg(buffer, why),
    };
    let deleted = client
        .request::<methods::SessionDelete>(SessionDeleteParams {
            session_id: full_id.to_string(),
        })
        .await;
    match deleted {
        Ok(_) => tui_output::ok_msg(buffer, format!("Deleted session {}", &full_id[..8])),
        Err(e) => tui_output::err_msg(buffer, format!("Error: {e}")),
    }
}

/// The session `id` or an id prefix names, or why it can't be deleted.
///
/// The running session is checked after the prefix resolves, since `/resume`
/// shows short ids and that's what people type.
fn session_to_delete<'a>(id: &str, known: &[&'a str], current: &str) -> Result<&'a str, String> {
    let matches: Vec<&'a str> = known
        .iter()
        .copied()
        .filter(|s| s.starts_with(id))
        .collect();
    match matches[..] {
        _ if id == current => Err("Cannot delete the current session.".into()),
        [] => Err(format!("No session found matching '{id}'.")),
        [only] if only == current => Err("Cannot delete the current session.".into()),
        [only] => Ok(only),
        _ => Err(format!(
            "Ambiguous: '{id}' matches {} sessions. Be more specific.",
            matches.len()
        )),
    }
}

async fn handle_resume_session(
    buffer: &mut ScrollBuffer,
    session: &mut TuiSession,
    client: &koda_client::InProcessClient,
    id: &str,
    mode: (&mut TrustMode, &SandboxState),
    renderer: &mut TuiRenderer,
) {
    use tui_output::GREEN;
    if session.id.starts_with(id) {
        tui_output::dim_msg(buffer, "Already in this session.".into());
    } else {
        match crate::session_id::all_sessions(client)
            .await
            .map_err(|e| e.to_string())
        {
            Ok(sessions) => {
                let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(id)).collect();
                match matches.len() {
                    0 => tui_output::err_msg(buffer, format!("No session found matching '{id}'.")),
                    1 => {
                        let target = &matches[0];
                        // Auto needs the host's sandbox: refused here,
                        // before switching, as the host would refuse it.
                        if target.mode == Some(TrustMode::Auto) && !mode.1.available {
                            tui_output::err_msg(
                                buffer,
                                format!(
                                    "Cannot resume session in Auto mode: {}",
                                    crate::mode::auto_refused(mode.1)
                                ),
                            );
                            return;
                        }
                        // The TUI opens it on the host before the next turn.
                        session.id = target.id.clone();
                        let short_id = target.id[..8].to_string();
                        let title_part = target
                            .name
                            .as_deref()
                            .map(|t| format!(" — {}", t.chars().take(40).collect::<String>()))
                            .unwrap_or_default();
                        let detail = format!(
                            "{title_part}  {}",
                            crate::util::unix_minutes(target.updated_at)
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
                        // Its stored mode (#590).
                        if let Some(stored) = target.mode {
                            *mode.0 = stored;
                        }
                        // Its earlier turns, as they were drawn live.
                        crate::tui_context::show_earlier_turns(
                            client, &target.id, renderer, buffer,
                        )
                        .await;
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
        ("Shift+Tab", "Cycle the mode: Safe, Auto, Plan"),
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

/// `/copy [n]`: copy the Nth-most-recent reply of the session's own to the
/// clipboard (1 = most recent). The replies are the ones the TUI was sent,
/// live or with the session's earlier turns, as Codex's `/copy` copies its
/// last agent message: the clipboard is the client's.
fn handle_copy_response(buffer: &mut ScrollBuffer, replies: &[String], n: usize) {
    let responses: Vec<&str> = replies.iter().map(String::as_str).collect();
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

#[cfg(test)]
mod tests {
    use super::session_to_delete;

    const CURRENT: &str = "aaaa1111-running";
    const OTHER: &str = "bbbb2222-finished";
    const NEIGHBOUR: &str = "bbbb3333-finished";

    fn resolve(id: &str) -> Result<&'static str, String> {
        session_to_delete(id, &[CURRENT, OTHER, NEIGHBOUR], CURRENT)
    }

    #[test]
    fn the_running_session_is_refused_by_full_id_or_prefix() {
        for id in [CURRENT, "aaaa1111", "aaaa"] {
            let refused = resolve(id).unwrap_err();
            assert!(refused.contains("current session"), "{id}: {refused}");
        }
    }

    #[test]
    fn another_session_resolves_from_a_prefix() {
        assert_eq!(resolve("bbbb2222"), Ok(OTHER));
        assert_eq!(resolve(OTHER), Ok(OTHER));
    }

    #[test]
    fn an_unknown_or_ambiguous_prefix_deletes_nothing() {
        assert!(resolve("cccc").unwrap_err().contains("No session"));
        assert!(resolve("bbbb").unwrap_err().contains("Ambiguous"));
        assert!(resolve("").unwrap_err().contains("Ambiguous"));
    }
}
