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

use crate::tui_context::TuiSession;
use koda_core::agent::KodaAgent;
use koda_core::config::KodaConfig;
use koda_core::providers::LlmProvider;
use ratatui::text::{Line, Span};
use std::sync::Arc;
use tokio::sync::RwLock;

pub enum SlashAction {
    Continue,
    Quit,
    OpenKeyMenu,
    /// `/model <name>`: the host changes the session's model.
    ChangeModel(String),
    /// Compact the session on the host, as a turn the TUI follows.
    Compact,
}

use tui_output::{BOLD, CYAN, DIM};

// ── Main handler ────────────────────────────────────────

#[allow(clippy::too_many_arguments, unused_variables)]
pub async fn handle_slash_command(
    buffer: &mut ScrollBuffer,
    input: &str,
    config: &mut KodaConfig,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
    session: &mut TuiSession,
    shared_mode: &trust::SharedTrustMode,
    renderer: &mut TuiRenderer,
    project_root: &std::path::Path,
    agent: &Arc<KodaAgent>,
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

        "/purge" => {
            crate::tui_wizards::handle_purge(buffer, client, arg, menu).await;
            SlashAction::Continue
        }

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

        "/agent" => {
            crate::tui_wizards::handle_list_agents(buffer, project_root);
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
                    handle_resume_session(buffer, session, client, &id, shared_mode).await;
                }
                // Bare ID shorthand: /resume <hex-id>
                Some(id)
                    if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') =>
                {
                    handle_resume_session(buffer, session, client, id, shared_mode).await;
                }
                _ => {} // no-op: session list is opened by the TUI context
            }
            SlashAction::Continue
        }

        "/memory" => {
            crate::tui_wizards::handle_memory(buffer, arg, project_root);
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

        _ => {
            tui_output::warn_msg(buffer, format!("Unknown command: {cmd}. Try /help."));
            SlashAction::Continue
        }
    }
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

/// The project's stored sessions, newest first, from the host.
pub(crate) async fn list_sessions(
    client: &koda_client::InProcessClient,
    limit: u32,
) -> Result<Vec<koda_client::protocol::session::Session>, String> {
    use koda_client::protocol::{methods, session::SessionListParams};
    client
        .request::<methods::SessionList>(SessionListParams {
            cwd: None,
            limit: Some(limit),
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
    let sessions = match list_sessions(client, 100).await {
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
    shared_mode: &trust::SharedTrustMode,
) {
    use tui_output::GREEN;
    if session.id.starts_with(id) {
        tui_output::dim_msg(buffer, "Already in this session.".into());
    } else {
        match list_sessions(client, 100).await {
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
async fn handle_copy_response(buffer: &mut ScrollBuffer, session: &TuiSession, n: usize) {
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
async fn handle_debug_bundle(buffer: &mut ScrollBuffer, session: &TuiSession, config: &KodaConfig) {
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
        provider: Some(config.provider_type.meta().name),
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
