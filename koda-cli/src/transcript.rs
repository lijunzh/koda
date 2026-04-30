//! Conversation transcript generator.
//!
//! Converts a session's `Message` slice into a Markdown document for
//! clipboard copy or file export. Two modes:
//!
//! - **Verbose** (default) — full fidelity: timestamps, token counts,
//!   all tool output (including Bash), session metadata header.
//! - **Summary** (`--summary`) — concise, human-readable: hides Bash
//!   output, omits token counts and timestamps.
//!
//! ## Verbose format
//!
//! ```text
//! # Koda Session — My Task
//!
//! | Field | Value |
//! |---|---|
//! | Session | abc123 |
//! | Model | claude-sonnet-4-20250514 |
//! | Started | 2026-04-10 10:32 UTC |
//!
//! ---
//!
//! ## 🧑 User  <sub>10:32:01</sub>
//! What does this function do?
//!
//! ## 🤖 Assistant  <sub>10:32:05</sub>
//! The function `foo()` does …
//!
//! <sub>tokens: 1234 prompt · 567 completion · 890 cache-read</sub>
//!
//! ### 📄 **Read** `src/main.rs`
//!
//! (full tool output shown)
//! ```

use koda_core::persistence::{Message, Role, SessionEvent, session_event_kind};
use koda_core::tools::{ToolEffect, classify_tool};
use std::collections::HashMap;

/// Session metadata for the verbose transcript header.
///
/// Assembled by the caller (`handle_export`) from live config — not stored
/// in the DB (see #878 design discussion).
#[derive(Debug, Default)]
pub struct SessionMeta {
    pub session_id: String,
    pub title: Option<String>,
    pub started_at: Option<String>,
    pub model: String,
    pub provider: String,
    pub project_root: String,
}

/// Maximum content lines to include per tool result in summary mode.
const SUMMARY_RESULT_LINES: usize = 10;

/// Maximum content lines to include per tool result in verbose mode.
const VERBOSE_RESULT_LINES: usize = 50;

/// Bash command truncation limit in summary mode.
const SUMMARY_BASH_CHARS: usize = 80;

/// Bash command truncation limit in verbose mode (effectively unlimited).
const VERBOSE_BASH_CHARS: usize = 500;

/// Env-var name for opting out of markdown hyperlinks in transcripts.
///
/// Default behavior: emit hyperlinks. Setting this to `"off"`, `"0"`,
/// or `"false"` disables them so paths render as plain text. Useful for
/// downstream consumers that munge markdown links into something less
/// readable than the bare path.
const HYPERLINK_KILL_SWITCH: &str = "KODA_TRANSCRIPT_HYPERLINKS";

fn hyperlinks_enabled() -> bool {
    // **#1109 F1**: read via runtime_env so tests can flip this without
    // `unsafe { std::env::set_var }`.
    !matches!(
        koda_core::runtime_env::get(HYPERLINK_KILL_SWITCH).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// Format the current UTC time as `YYYY-MM-DD HH:MM UTC` for the transcript header.
fn format_utc_now() -> String {
    let dt = crate::util::utc_now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
    )
}

/// Extract `(id, function_name, arguments_json)` from one element of
/// the parsed `tool_calls` JSON array.
///
/// Production code persists tool calls via
/// `serde_json::to_string(&Vec<ToolCall>)`, which produces the FLAT
/// shape `{"id":..., "function_name":..., "arguments":...}` (see
/// `koda_core::providers::ToolCall`). Pre-#1108 the renderer here
/// looked at the OpenAI-NESTED shape `{"function": {"name":..., }}`,
/// silently fell through to defaults, and rendered every export as
/// `### 🔧 **Tool**` with no name.
///
/// Read the flat shape first; fall back to nested for any legacy
/// data or test fixtures still using the OpenAI shape. This mirrors
/// the established pattern in `microcompact.rs` and
/// `context_analysis.rs` — those modules read tool_calls JSON the
/// right way; this one was the lone offender.
fn extract_tool_call_meta(call: &serde_json::Value) -> (String, String, String) {
    let id = call
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = call
        .get("function_name")
        .or_else(|| call.get("function").and_then(|f| f.get("name")))
        .and_then(|v| v.as_str())
        .unwrap_or("Tool")
        .to_string();
    let args = call
        .get("arguments")
        .or_else(|| call.get("function").and_then(|f| f.get("arguments")))
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();
    (id, name, args)
}

/// Generate a Markdown transcript from a slice of session messages.
///
/// - `verbose = true` (default): full fidelity — timestamps, token counts,
///   all tool output, session metadata header.
/// - `verbose = false` (`--summary`): concise, human-readable.
///
/// `events` carries non-message engine events persisted to the
/// `session_events` table (#1108 P1b/P2a). They split into two
/// rendering buckets:
/// - **Sub-agent events** (`parent_tool_call_id = Some(id)`) are
///   folded as a collapsible `<details>` block under the matching
///   `Tool` result. Pass an empty slice for sessions with no bg
///   sub-agents.
/// - **Top-level events** (`parent_tool_call_id = None`) are appended
///   in a chronological "Background activity" section at the end,
///   so the reader can correlate microcompact / rate-limit / bg-task
///   transitions with the conversation above.
///
/// Returns the transcript as a `String`. The caller is responsible for
/// writing it to the clipboard or a file.
pub fn render(
    messages: &[Message],
    events: &[SessionEvent],
    meta: &SessionMeta,
    verbose: bool,
) -> String {
    let mut out = String::with_capacity(if verbose { 16384 } else { 4096 });

    // Header
    let title = meta.title.as_deref().unwrap_or("Koda Session");
    let now = format_utc_now();
    out.push_str(&format!("# {title} — {now}\n\n"));

    if verbose {
        render_metadata_table(&mut out, meta);
    }

    // Build tool_call_id → tool_name mapping for result correlation
    let mut tool_id_to_name: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if msg.role == Role::Assistant
            && let Some(ref tc_json) = msg.tool_calls
            && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
        {
            for call in calls {
                let (id, name, _args) = extract_tool_call_meta(&call);
                if !id.is_empty() && name != "Tool" {
                    tool_id_to_name.insert(id, name);
                }
            }
        }
    }

    // #1108 P2a: bucket sub-agent events by parent tool_call_id so
    // each Tool result can render its folded trace in O(1). Top-level
    // events (no parent) accumulate in a separate Vec for the
    // "Background activity" tail section. Single pass, two buckets.
    let mut events_by_parent: HashMap<&str, Vec<&SessionEvent>> = HashMap::new();
    let mut top_level_events: Vec<&SessionEvent> = Vec::new();
    for ev in events {
        match ev.parent_tool_call_id.as_deref() {
            Some(parent) => events_by_parent.entry(parent).or_default().push(ev),
            None => top_level_events.push(ev),
        }
    }

    for msg in messages {
        match msg.role {
            Role::System => {} // Skip — internal plumbing

            Role::User => {
                out.push_str("---\n\n");
                render_role_header(&mut out, "\u{1f9d1} User", msg, verbose);
                if let Some(ref content) = msg.content {
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
            }

            Role::Assistant => {
                render_role_header(&mut out, "🤖 Assistant", msg, verbose);

                // Thinking block (Claude extended thinking) — before text
                if let Some(ref thinking) = msg.thinking_content
                    && !thinking.trim().is_empty()
                {
                    out.push_str("> 💭 **Thinking**\n");
                    for line in thinking.trim().lines() {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push('\n');
                }

                // Text content
                if let Some(ref content) = msg.content {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        out.push_str(trimmed);
                        out.push_str("\n\n");
                    }
                }

                // Tool call headers
                if let Some(ref tc_json) = msg.tool_calls
                    && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
                {
                    let bash_limit = if verbose {
                        VERBOSE_BASH_CHARS
                    } else {
                        SUMMARY_BASH_CHARS
                    };
                    let link = hyperlinks_enabled();
                    for call in &calls {
                        let (id, name, args_json) = extract_tool_call_meta(call);
                        let detail = tool_detail_markdown(
                            &name,
                            &args_json,
                            bash_limit,
                            &meta.project_root,
                            link,
                        );
                        let icon = tool_icon(&name);
                        out.push_str(&format!("### {icon} **{name}**"));
                        if !detail.is_empty() {
                            // Detail may already contain markdown link
                            // syntax (`[disp](uri)`); only the plain-text
                            // branches are wrapped in backticks. Linked
                            // detail is left bare so the link renders.
                            if detail.starts_with('[') {
                                out.push(' ');
                                out.push_str(&detail);
                            } else {
                                out.push_str(&format!(" `{detail}`"));
                            }
                        }
                        // P1a (#1108): surface tool_call_id so a reader
                        // can correlate parallel calls with their
                        // `Output` rows below. Skip for empty ids —
                        // older sessions may not have them.
                        if !id.is_empty() {
                            out.push_str(&format!(" `{id}`"));
                        }
                        out.push('\n');
                    }
                    out.push('\n');
                }

                // Token counts (verbose only)
                if verbose {
                    render_token_counts(&mut out, msg);
                }
            }

            Role::Tool => {
                let tool_name = msg
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_id_to_name.get(id))
                    .map(|s| s.as_str())
                    .unwrap_or("");

                let content = msg.content.as_deref().unwrap_or("").trim();
                let total_lines = content.lines().count();

                if !content.is_empty() {
                    let effect = classify_tool(tool_name);
                    let max_lines = if verbose {
                        VERBOSE_RESULT_LINES
                    } else {
                        SUMMARY_RESULT_LINES
                    };

                    // Verbose: show all tool output. Summary: only read-only.
                    let show_content = verbose || effect == ToolEffect::ReadOnly;

                    if show_content {
                        // P1a (#1108): include tool_call_id so parallel
                        // call/result pairs can be matched. The `id`
                        // appears on the matching call's `### **Tool**`
                        // header above, so the reader can grep for it.
                        let header = match msg.tool_call_id.as_deref() {
                            Some(id) if !id.is_empty() => {
                                format!("**Output for `{id}`:**\n\n```\n")
                            }
                            _ => "**Output:**\n\n```\n".to_string(),
                        };
                        out.push_str(&header);
                        let preview_lines: Vec<&str> = content.lines().take(max_lines).collect();
                        out.push_str(&preview_lines.join("\n"));
                        if total_lines > max_lines {
                            out.push_str(&format!("\n… ({} more lines)", total_lines - max_lines));
                        }
                        out.push_str("\n```\n\n");
                    } else if total_lines > 0 {
                        out.push_str(&format!(
                            "> _{total_lines} line(s) of output \u{2014} run tool to see full result_\n\n"
                        ));
                    }
                }

                // #1108 P2a: fold the bg sub-agent's narrative trace
                // under its `InvokeAgent` tool result. Pre-#1108 the
                // trace was sink-only and never made it into the
                // export. Use a `<details>` block so the trace is
                // hidden by default in rendered Markdown viewers but
                // still grep-able for debugging. Skipped silently if
                // no events match this tool_call_id (the common case
                // for non-`InvokeAgent` tools).
                if let Some(call_id) = msg.tool_call_id.as_deref()
                    && let Some(events) = events_by_parent.get(call_id)
                    && !events.is_empty()
                {
                    out.push_str(&format!(
                        "<details><summary>\u{1f50d} Sub-agent trace ({} event{})</summary>\n\n",
                        events.len(),
                        if events.len() == 1 { "" } else { "s" },
                    ));
                    out.push_str("```\n");
                    for ev in events {
                        out.push_str(&ev.payload);
                        out.push('\n');
                    }
                    out.push_str("```\n\n</details>\n\n");
                }
            }
        }
    }

    // #1108 P1b: append top-level engine events as a tail section
    // (microcompact, rate-limit, bg-task transitions, etc.). These
    // are the events with no `parent_tool_call_id` — sub-agent ones
    // already rendered above under their `Tool` results. Skipped if
    // empty so non-bg sessions stay visually clean.
    if !top_level_events.is_empty() {
        render_background_activity(&mut out, &top_level_events);
    }

    out
}

/// Render the trailing "Background activity" section.
///
/// One bullet per event, kind-prefixed so the reader can scan for
/// task-state transitions vs. info messages without parsing JSON.
/// Bg-task updates are pretty-printed (`agent:N: Pending → Running`)
/// because raw JSON in a transcript is illegible noise.
///
/// **Iter-heartbeat aggregation (#1158 d)**: by default, per-iteration
/// `Running { iter: N>0 }` heartbeats are dropped from the export and
/// summarised into one trailing line per task (`agent:N: ran 9
/// iterations → Completed`). The state transitions (Pending,
/// Running with iter==0, Cancelled, Completed, Errored) and the iter-0
/// "started" marker are preserved verbatim. Set `KODA_EXPORT_VERBOSE=1`
/// to disable the filter and re-emit every heartbeat (debugging).
fn render_background_activity(out: &mut String, events: &[&SessionEvent]) {
    out.push_str("---\n\n## \u{1f4ca} Background activity\n\n");
    out.push_str(
        "<sub>Engine events captured during the session (info messages, \
         bg-task state transitions). Pre-#1108 these were sink-only and \
         not exported.</sub>\n\n",
    );
    let verbose = std::env::var("KODA_EXPORT_VERBOSE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // Per-task heartbeat counters — only used when not verbose. Keyed
    // by task_id so multi-agent sessions get one summary line each.
    let mut iter_counts: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
    for ev in events {
        let ts = ev.created_at.as_deref().unwrap_or("");
        match ev.kind.as_str() {
            session_event_kind::INFO => {
                out.push_str(&format!("- `{ts}` \u{2139}\u{fe0f} {}\n", ev.payload));
            }
            session_event_kind::BG_TASK_UPDATE => {
                if !verbose
                    && let Some((tid, iter)) = parse_running_iter(&ev.payload)
                    && iter > 0
                {
                    // Drop heartbeat noise; tally for summary.
                    *iter_counts.entry(tid).or_insert(0) += 1;
                    continue;
                }
                let pretty =
                    pretty_bg_task_update(&ev.payload).unwrap_or_else(|| ev.payload.clone());
                out.push_str(&format!("- `{ts}` \u{1f680} {pretty}\n"));
            }
            other => {
                out.push_str(&format!("- `{ts}` `{other}` {}\n", ev.payload));
            }
        }
    }
    // Append per-task heartbeat summary (only fires when we actually
    // dropped heartbeats — no-op for verbose mode and for sessions
    // whose tasks never emitted iter>0).
    for (tid, count) in iter_counts {
        out.push_str(&format!(
            "- \u{1f4ad} agent:{tid}: {count} iteration{plural} aggregated \
             (set `KODA_EXPORT_VERBOSE=1` to expand)\n",
            plural = if count == 1 { "" } else { "s" },
        ));
    }
    out.push('\n');
}

/// Extract `(task_id, iter)` from a `BgTaskUpdate` payload whose
/// status is `Running { iter: N }`. Returns `None` for any other
/// status (Pending, Cancelled, Completed, Errored) or malformed JSON
/// — caller falls back to the regular pretty-print path.
fn parse_running_iter(payload: &str) -> Option<(u64, u32)> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let task_id = v.get("task_id")?.as_u64()?;
    let iter = v
        .get("status")?
        .as_object()?
        .get("Running")?
        .as_object()?
        .get("iter")?
        .as_u64()?;
    Some((task_id, iter as u32))
}

/// Best-effort pretty-printer for a `BgTaskUpdate` JSON payload.
///
/// Returns `None` on any parse failure so the caller falls back to
/// rendering the raw JSON — strictly better than dropping the row.
fn pretty_bg_task_update(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let task_id = v.get("task_id")?.as_u64()?;
    let status = v.get("status")?;
    // `AgentStatus` serializes as either a string (`"Pending"`) or
    // an object (`{"Running": {"iter": 3}}`). Handle both.
    let status_str = match status {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}({v})"))
            .collect::<Vec<_>>()
            .join(", "),
        _ => status.to_string(),
    };
    Some(format!("agent:{task_id}: {status_str}"))
}

// ── Verbose-mode helpers ─────────────────────────────────────────────

/// Render the metadata table at the top of a verbose transcript.
fn render_metadata_table(out: &mut String, meta: &SessionMeta) {
    out.push_str("| Field | Value |\n|---|---|\n");
    out.push_str(&format!("| Session | `{}` |\n", meta.session_id));
    out.push_str(&format!("| Model | {} |\n", meta.model));
    out.push_str(&format!("| Provider | {} |\n", meta.provider));
    out.push_str(&format!("| Project | `{}` |\n", meta.project_root));
    if let Some(ref started) = meta.started_at {
        out.push_str(&format!("| Started | {started} |\n"));
    }
    out.push('\n');
}

/// Render a role header with an optional timestamp (verbose mode).
fn render_role_header(out: &mut String, label: &str, msg: &Message, verbose: bool) {
    out.push_str(&format!("## {label}"));
    if verbose && let Some(ref ts) = msg.created_at {
        // Show HH:MM:SS portion if it looks like an ISO timestamp.
        let time_part = ts.split('T').nth(1).and_then(|t| t.get(..8)).unwrap_or(ts);
        out.push_str(&format!("  <sub>{time_part}</sub>"));
    }
    out.push_str("\n\n");
}

/// Render token counts after an assistant message (verbose mode).
fn render_token_counts(out: &mut String, msg: &Message) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(p) = msg.prompt_tokens {
        parts.push(format!("{p} prompt"));
    }
    if let Some(c) = msg.completion_tokens {
        parts.push(format!("{c} completion"));
    }
    if let Some(cr) = msg.cache_read_tokens
        && cr > 0
    {
        parts.push(format!("{cr} cache-read"));
    }
    if let Some(cc) = msg.cache_creation_tokens
        && cc > 0
    {
        parts.push(format!("{cc} cache-write"));
    }
    if let Some(t) = msg.thinking_tokens
        && t > 0
    {
        parts.push(format!("{t} thinking"));
    }
    if !parts.is_empty() {
        out.push_str(&format!("<sub>tokens: {}</sub>\n\n", parts.join(" · ")));
    }
}

/// Human-readable icon for each tool type.
fn tool_icon(name: &str) -> &'static str {
    match name {
        "Read" => "📄",
        "Write" => "✏️",
        "Edit" => "✏️",
        "Delete" => "🗑️",
        "Bash" => "💻",
        "Grep" => "🔍",
        "List" | "Glob" => "📁",
        "WebFetch" => "🌐",
        "TodoWrite" => "📋",
        "MemoryWrite" | "MemoryRead" => "🧠",
        "InvokeAgent" => "🤖",
        "AskUser" => "💬",
        _ => "🔧",
    }
}

/// One-line summary of a tool call's arguments, with file paths and URLs
/// rendered as **markdown links** when hyperlinks are enabled.
///
/// Per-tool dispatch is delegated to [`crate::tool_header::detail_text`]
/// so the same `(name, args)` pair always produces the same human-readable
/// summary across the live TUI, history replay, and transcript export.
/// This wrapper only adds the markdown-link layer on top of the shared
/// plain-text summary.
///
/// `bash_limit` controls Bash command truncation (80 in summary, 500 in verbose).
/// `project_root` is used to resolve relative file paths into absolute
/// `file:///` URIs; if empty or the path is already absolute, no resolution
/// is performed.
fn tool_detail_markdown(
    name: &str,
    args_json: &str,
    bash_limit: usize,
    project_root: &str,
    link: bool,
) -> String {
    let args: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    let raw = crate::tool_header::detail_text(name, &args, bash_limit);
    if !link || raw.is_empty() {
        return raw;
    }
    match name {
        "Read" | "Write" | "Edit" | "Delete" => {
            // `raw` is the file path; wrap as [path](file:///abs).
            let abs = absolute_path(&raw, project_root);
            format!("[{raw}]({})", file_uri(&abs))
        }
        "WebFetch" => format!("[{raw}]({raw})"),
        // Grep / Glob / List / Bash / generic: leave as plain text.
        // The directory portion of Grep is intentionally NOT linked
        // because the eye-catching part is the pattern, not the dir.
        _ => raw,
    }
}

/// Resolve `path` against `project_root` if relative; pass through if absolute.
///
/// We deliberately don't try to canonicalize (the file may not exist on
/// the machine viewing the transcript). The resulting string is best-effort.
fn absolute_path(path: &str, project_root: &str) -> String {
    if path.starts_with('/') || project_root.is_empty() {
        return path.to_string();
    }
    let root = project_root.trim_end_matches('/');
    format!("{root}/{path}")
}

/// Build a `file:///` URI from an absolute path with light percent-encoding.
///
/// Encodes the characters most likely to break markdown link parsing:
/// space, parenthesis, square bracket. Other characters are passed through
/// — most viewers tolerate them, and full percent-encoding would pull in a
/// dependency for marginal benefit.
fn file_uri(abs_path: &str) -> String {
    let mut out = String::with_capacity(abs_path.len() + 8);
    out.push_str("file://");
    if !abs_path.starts_with('/') {
        out.push('/');
    }
    for ch in abs_path.chars() {
        match ch {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::persistence::Message;

    /// Serializes tests that depend on `HYPERLINK_KILL_SWITCH`.
    ///
    /// `kill_switch_disables_hyperlinks` mutates the env var while the
    /// other tests in this module just read it via `hyperlinks_enabled()`.
    /// Since env vars are process-global and `cargo test` runs tests in
    /// parallel by default, the writer can flip the var to "off" mid-read
    /// of any other test that calls `render()` and asserts on hyperlinks.
    /// On macOS this raced ~50% of the time and blocked PR #1107.
    ///
    /// Lock acquisition: every test that depends on the env var's value
    /// (one writer, three readers) takes this lock for its full duration.
    /// Other transcript tests don't assert on hyperlinks at all (they
    /// look at headers, paths, code blocks, etc.) so they don't need the
    /// lock and can keep running in parallel.
    static HYPERLINK_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn make_msg(role: Role, content: &str) -> Message {
        Message {
            id: 0,
            session_id: "test".into(),
            role,
            content: Some(content.into()),
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

    fn default_meta() -> SessionMeta {
        SessionMeta {
            session_id: "test-session".into(),
            title: None,
            started_at: None,
            model: "test-model".into(),
            provider: "test-provider".into(),
            project_root: "/tmp/project".into(),
        }
    }

    #[test]
    fn empty_messages_produces_header_only() {
        let meta = SessionMeta {
            title: Some("Test Session".into()),
            ..default_meta()
        };
        let out = render(&[], &[], &meta, false);
        assert!(out.contains("# Test Session"));
        assert!(!out.contains("🧑 User"));
    }

    #[test]
    fn user_message_renders_correctly() {
        let msgs = vec![make_msg(Role::User, "hello koda")];
        let out = render(&msgs, &[], &default_meta(), false);
        assert!(out.contains("🧑 User"));
        assert!(out.contains("hello koda"));
    }

    #[test]
    fn assistant_message_renders_correctly() {
        let msgs = vec![make_msg(Role::Assistant, "I can help!")];
        let out = render(&msgs, &[], &default_meta(), false);
        assert!(out.contains("🤖 Assistant"));
        assert!(out.contains("I can help!"));
    }

    #[test]
    fn system_messages_skipped() {
        let msgs = vec![
            make_msg(Role::System, "secret prompt"),
            make_msg(Role::User, "hi"),
        ];
        let out = render(&msgs, &[], &default_meta(), false);
        assert!(!out.contains("secret prompt"));
    }

    #[test]
    fn tool_read_result_shown_as_code_block() {
        let mut result_msg = make_msg(Role::Tool, "fn main() {}\n");
        result_msg.tool_call_id = Some("call_1".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(flat_tool_calls_json(&[(
            "call_1",
            "Read",
            r#"{"file_path":"src/main.rs"}"#,
        )]));

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &[], &default_meta(), false);
        assert!(out.contains("```"));
        assert!(out.contains("fn main()"));
    }

    #[test]
    fn bash_result_shows_summary_not_content() {
        let mut result_msg = make_msg(Role::Tool, "line1\nline2\nline3");
        result_msg.tool_call_id = Some("call_2".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(flat_tool_calls_json(&[(
            "call_2",
            "Bash",
            r#"{"command":"ls"}"#,
        )]));

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &[], &default_meta(), false);
        // Bash is mutating → summarised in summary mode, not shown verbatim
        assert!(!out.contains("line1"));
        assert!(out.contains("3 line(s) of output"));
    }

    #[test]
    fn thinking_content_renders_as_blockquote() {
        // thinking_content is Claude's chain-of-thought — it is intentionally
        // included in the exported transcript as a blockquote so the user can
        // review the model's reasoning (#819).
        let mut msg = make_msg(Role::Assistant, "The answer is 42.");
        msg.thinking_content = Some("Let me think step by step: 6 x 7 = 42.".into());

        let out = render(&[msg], &[], &default_meta(), false);
        assert!(
            out.contains("The answer is 42."),
            "response text must appear"
        );
        assert!(
            out.contains("Thinking"),
            "thinking block header must appear in transcript"
        );
        assert!(
            out.contains("Let me think step by step"),
            "thinking content must appear in transcript",
        );
    }

    #[test]
    fn verbose_header_includes_metadata() {
        let meta = SessionMeta {
            session_id: "sess-42".into(),
            title: Some("Debug Session".into()),
            started_at: Some("2026-04-14T12:00:00Z".into()),
            model: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            project_root: "/home/user/project".into(),
        };
        let out = render(&[], &[], &meta, true);
        assert!(out.contains("sess-42"), "session ID in header");
        assert!(out.contains("claude-sonnet-4-20250514"), "model in header");
        assert!(out.contains("anthropic"), "provider in header");
        assert!(out.contains("/home/user/project"), "project root in header");
    }

    #[test]
    fn verbose_shows_token_counts() {
        let mut msg = make_msg(Role::Assistant, "The answer.");
        msg.prompt_tokens = Some(100);
        msg.completion_tokens = Some(50);
        msg.cache_read_tokens = Some(80);

        let out = render(&[msg], &[], &default_meta(), true);
        assert!(out.contains("100 prompt"), "prompt tokens shown");
        assert!(out.contains("50 completion"), "completion tokens shown");
        assert!(out.contains("80 cache-read"), "cache-read tokens shown");
    }

    #[test]
    fn summary_hides_token_counts() {
        let mut msg = make_msg(Role::Assistant, "The answer.");
        msg.prompt_tokens = Some(100);
        msg.completion_tokens = Some(50);

        let out = render(&[msg], &[], &default_meta(), false);
        assert!(!out.contains("100 prompt"));
    }

    #[test]
    fn verbose_shows_timestamps() {
        let mut msg = make_msg(Role::User, "hello");
        msg.created_at = Some("2026-04-14T09:15:30Z".into());

        let out = render(&[msg], &[], &default_meta(), true);
        assert!(out.contains("09:15:30"), "timestamp shown in verbose");
    }

    #[test]
    fn summary_hides_timestamps() {
        let mut msg = make_msg(Role::User, "hello");
        msg.created_at = Some("2026-04-14T09:15:30Z".into());

        let out = render(&[msg], &[], &default_meta(), false);
        assert!(!out.contains("09:15:30"), "timestamp hidden in summary");
    }

    #[test]
    fn bash_result_shown_in_verbose_mode() {
        let mut result_msg = make_msg(Role::Tool, "line1\nline2\nline3");
        result_msg.tool_call_id = Some("call_3".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(flat_tool_calls_json(&[(
            "call_3",
            "Bash",
            r#"{"command":"ls"}"#,
        )]));

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &[], &default_meta(), true);
        // Verbose mode shows all tool output, including Bash
        assert!(out.contains("line1"));
        assert!(out.contains("line3"));
    }

    // ── Markdown hyperlink emission ────────────────────────────────

    /// Build a `tool_calls` JSON string the way production code does it:
    /// `serde_json::to_string(&Vec<ToolCall>)` — the FLAT shape with
    /// `function_name` and `arguments` at the top level.
    ///
    /// Pre-#1108 every transcript test built a *different* (nested OpenAI)
    /// shape via `json!({"function": {...}})`. Tests passed in CI while
    /// every real export rendered tool calls as `### 🔧 **Tool**`.
    /// New fixtures MUST go through this helper so test data matches
    /// production data and the bug class can't recur.
    fn flat_tool_calls_json(calls: &[(&str, &str, &str)]) -> String {
        use koda_core::providers::ToolCall;
        let toolcalls: Vec<ToolCall> = calls
            .iter()
            .map(|(id, name, args)| ToolCall {
                id: (*id).to_string(),
                function_name: (*name).to_string(),
                arguments: (*args).to_string(),
                thought_signature: None,
            })
            .collect();
        serde_json::to_string(&toolcalls).expect("ToolCall serializes")
    }

    /// Build an assistant message with a single tool call.
    ///
    /// Routes through [`flat_tool_calls_json`] so the resulting
    /// `tool_calls` field matches what production code persists.
    fn assistant_with_call(name: &str, args_json: &str) -> Message {
        let mut m = make_msg(Role::Assistant, "");
        m.tool_calls = Some(flat_tool_calls_json(&[("c1", name, args_json)]));
        m
    }

    #[test]
    fn read_path_emits_markdown_link_with_file_uri() {
        let _g = HYPERLINK_ENV_LOCK.lock().unwrap();
        let msg = assistant_with_call("Read", r#"{"file_path":"src/main.rs"}"#);
        let meta = SessionMeta {
            project_root: "/home/user/proj".into(),
            ..default_meta()
        };
        let out = render(&[msg], &[], &meta, false);
        assert!(
            out.contains("[src/main.rs](file:///home/user/proj/src/main.rs)"),
            "relative path should resolve under project_root, got:\n{out}"
        );
    }

    #[test]
    fn absolute_read_path_skips_root_join() {
        let _g = HYPERLINK_ENV_LOCK.lock().unwrap();
        let msg = assistant_with_call("Read", r#"{"file_path":"/etc/hosts"}"#);
        let meta = SessionMeta {
            project_root: "/home/user/proj".into(),
            ..default_meta()
        };
        let out = render(&[msg], &[], &meta, false);
        assert!(
            out.contains("[/etc/hosts](file:///etc/hosts)"),
            "absolute path should pass through, got:\n{out}"
        );
    }

    #[test]
    fn webfetch_url_becomes_self_link() {
        let _g = HYPERLINK_ENV_LOCK.lock().unwrap();
        let msg = assistant_with_call("WebFetch", r#"{"url":"https://example.com/x"}"#);
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(
            out.contains("[https://example.com/x](https://example.com/x)"),
            "URL should be a markdown self-link, got:\n{out}"
        );
    }

    #[test]
    fn bash_detail_stays_plain_codespan() {
        // Bash commands aren't paths or URLs — keep them in backticks
        // so monospace formatting (and shell tokens) survive.
        let msg = assistant_with_call("Bash", r#"{"command":"git status"}"#);
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(out.contains("`git status`"), "got:\n{out}");
        assert!(
            !out.contains("](git status)"),
            "bash should never be linked"
        );
    }

    #[test]
    fn grep_detail_stays_plain_codespan() {
        let msg = assistant_with_call("Grep", r#"{"search_string":"TODO","directory":"src"}"#);
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(out.contains("`\"TODO\" in src`"), "got:\n{out}");
    }

    #[test]
    fn kill_switch_disables_hyperlinks() {
        let _g = HYPERLINK_ENV_LOCK.lock().unwrap();
        // **#1109 F1**: was `unsafe { std::env::set_var }` with snapshot/restore.
        // Now uses [`koda_core::runtime_env`] — thread-safe, no UB, no
        // `std::env` mutation. The HYPERLINK_ENV_LOCK still serializes us
        // against parallel reader tests in this binary.
        koda_core::runtime_env::set(HYPERLINK_KILL_SWITCH, "off");
        let msg = assistant_with_call("Read", r#"{"file_path":"/x.rs"}"#);
        let out = render(&[msg], &[], &default_meta(), false);
        koda_core::runtime_env::remove(HYPERLINK_KILL_SWITCH);
        assert!(out.contains("`/x.rs`"), "plain text expected, got:\n{out}");
        assert!(!out.contains("file:///"), "link should be suppressed");
    }

    #[test]
    fn dry_equivalence_with_tool_header_detail_text() {
        // Regression guard: transcript detail strings must agree with the
        // shared `tool_header::detail_text` for every supported tool. If
        // someone forks the dispatch back into this module, this test fails.
        use crate::tool_header::detail_text;
        let cases: Vec<(&str, serde_json::Value, usize)> = vec![
            ("Read", serde_json::json!({"file_path": "a.rs"}), 80),
            ("Bash", serde_json::json!({"command": "echo hi"}), 80),
            (
                "Grep",
                serde_json::json!({"search_string": "x", "directory": "."}),
                80,
            ),
            ("Glob", serde_json::json!({"pattern": "**/*.rs"}), 80),
            ("List", serde_json::json!({"directory": "src"}), 80),
            ("WebFetch", serde_json::json!({"url": "https://x"}), 80),
        ];
        for (name, args, bash) in cases {
            let from_helper = detail_text(name, &args, bash);
            // tool_detail_markdown with link=false is a pure pass-through.
            let from_transcript = tool_detail_markdown(name, &args.to_string(), bash, "", false);
            assert_eq!(
                from_helper, from_transcript,
                "transcript detail must match tool_header::detail_text for {name}"
            );
        }
    }

    #[test]
    fn file_uri_percent_encodes_breaking_chars() {
        // Spaces and brackets break naive markdown link parsers — percent-
        // encode the most common offenders so links survive copy/paste.
        let uri = file_uri("/My Files/[draft].md");
        assert_eq!(uri, "file:///My%20Files/%5Bdraft%5D.md");
    }

    // ── #1108 P0/P1a: tool name + args + call_id surfacing ────────────

    /// Regression test for #1108 P0: real production tool_calls JSON
    /// (the flat `function_name` shape from `serde_json::to_string(
    /// &Vec<ToolCall>)`) MUST surface the tool name in the rendered
    /// markdown header. Pre-fix this test would have failed with
    /// `### 🔧 **Tool**` — every export silently lied about which
    /// tool was called for the entire history of the export feature.
    #[test]
    fn production_tool_calls_json_renders_tool_name_in_header() {
        let mut msg = make_msg(Role::Assistant, "");
        msg.tool_calls = Some(flat_tool_calls_json(&[(
            "call_xyz",
            "InvokeAgent",
            r#"{"agent_name":"explore","prompt":"map the repo"}"#,
        )]));
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(
            out.contains("**InvokeAgent**"),
            "production-shape tool_calls must render the tool NAME in the header. \
             Pre-#1108 every real export rendered `### 🔧 **Tool**` because the \
             renderer read the OpenAI-nested shape (`function.name`) while \
             persistence wrote the flat shape (`function_name`). got:\n{out}"
        );
        assert!(
            !out.contains("**Tool**"),
            "`**Tool**` is the silent fallback that masked the bug for months. \
             It should never appear when the call has a real function_name. \
             got:\n{out}"
        );
    }

    /// Companion test: production-shape tool_calls must also expose the
    /// arguments to the `tool_header::detail_text` formatter. Pre-#1108
    /// args were silently `"{}"` so detail rendered as `🔧 **Tool**`
    /// with no path/command suffix.
    #[test]
    fn production_tool_calls_json_renders_tool_args_in_header() {
        let mut msg = make_msg(Role::Assistant, "");
        msg.tool_calls = Some(flat_tool_calls_json(&[(
            "call_zzz",
            "Read",
            r#"{"file_path":"src/very_distinctive_file.rs"}"#,
        )]));
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(
            out.contains("very_distinctive_file.rs"),
            "production-shape tool_calls must surface the arguments. \
             Pre-#1108 args were swallowed because the renderer read \
             `function.arguments` (nested) while persistence wrote \
             `arguments` (flat). got:\n{out}"
        );
    }

    /// Regression test for #1108 P1a: every tool call header must
    /// include the `tool_call_id` so a reader can correlate parallel
    /// calls (e.g. 3× `InvokeAgent` in one assistant turn) with their
    /// corresponding `Output` rows. Pre-fix the id only existed
    /// internally and never reached the export.
    #[test]
    fn tool_call_id_appears_in_header_for_correlation() {
        let mut msg = make_msg(Role::Assistant, "");
        msg.tool_calls = Some(flat_tool_calls_json(&[
            (
                "call_a",
                "InvokeAgent",
                r#"{"agent_name":"explore","prompt":"a"}"#,
            ),
            (
                "call_b",
                "InvokeAgent",
                r#"{"agent_name":"explore","prompt":"b"}"#,
            ),
        ]));
        let out = render(&[msg], &[], &default_meta(), false);
        assert!(
            out.contains("call_a") && out.contains("call_b"),
            "both tool_call_ids must appear in the header so parallel \
             InvokeAgent calls can be matched to their Output rows. \
             got:\n{out}"
        );
    }

    /// Regression test for #1108 P1a: when a `Tool` result message
    /// carries a `tool_call_id`, the `**Output**` header in the
    /// transcript must include it so it can be matched to its
    /// originating call.
    #[test]
    fn tool_call_id_appears_in_result_output_header() {
        let mut a = make_msg(Role::Assistant, "");
        a.tool_calls = Some(flat_tool_calls_json(&[(
            "call_corr",
            "Read",
            r#"{"file_path":"x.rs"}"#,
        )]));
        let mut t = make_msg(Role::Tool, "file contents here");
        t.tool_call_id = Some("call_corr".into());
        let out = render(&[a, t], &[], &default_meta(), false);
        assert!(
            out.contains("call_corr"),
            "the result row's Output header must mention its tool_call_id \
             so parallel call/result pairs can be matched. got:\n{out}"
        );
    }

    // ── #1108 P2a: sub-agent trace folding ────────────────────────────

    /// Helper: build a `SessionEvent` for renderer tests. The DB id
    /// and timestamp don't matter — the renderer only uses `kind`,
    /// `payload`, and `parent_tool_call_id`.
    fn ev(kind: &str, payload: &str, parent: Option<&str>) -> SessionEvent {
        SessionEvent {
            id: 0,
            session_id: "sess".into(),
            kind: kind.into(),
            payload: payload.into(),
            parent_tool_call_id: parent.map(str::to_string),
            created_at: Some("2026-04-27 06:00:00".into()),
        }
    }

    #[test]
    fn sub_agent_events_fold_under_matching_tool_result() {
        let mut a = make_msg(Role::Assistant, "");
        a.tool_calls = Some(flat_tool_calls_json(&[(
            "call_inv",
            "InvokeAgent",
            r#"{"agent_name":"explore","prompt":"go"}"#,
        )]));
        let mut t = make_msg(Role::Tool, "sub-agent finished");
        t.tool_call_id = Some("call_inv".into());

        let events = vec![
            ev(
                session_event_kind::SUB_AGENT_EVENT,
                "  \u{1f527} Read foo.rs",
                Some("call_inv"),
            ),
            ev(
                session_event_kind::SUB_AGENT_EVENT,
                "  \u{1f4ad} Looking at imports\u{2026}",
                Some("call_inv"),
            ),
        ];

        let out = render(&[a, t], &events, &default_meta(), true);
        assert!(
            out.contains("<details><summary>"),
            "sub-agent events must be folded in a <details> block. got:\n{out}"
        );
        assert!(
            out.contains("Sub-agent trace (2 events)"),
            "summary must show the folded event count"
        );
        assert!(out.contains("Read foo.rs"));
        assert!(out.contains("Looking at imports"));
    }

    #[test]
    fn sub_agent_events_skipped_when_no_matching_tool_call_id() {
        // Event has parent "call_X" but no tool result carries that
        // id — the event must be silently dropped from the per-tool
        // section (it'll surface in "Background activity" only if it
        // has no parent at all, which it does here).
        let mut t = make_msg(Role::Tool, "some output");
        t.tool_call_id = Some("call_OTHER".into());
        let events = vec![ev(
            session_event_kind::SUB_AGENT_EVENT,
            "orphan trace line",
            Some("call_X"),
        )];
        let out = render(&[t], &events, &default_meta(), true);
        assert!(
            !out.contains("orphan trace line"),
            "orphan parented events must not surface anywhere. got:\n{out}"
        );
        assert!(
            !out.contains("<details>"),
            "no details block when no events match the tool result"
        );
    }

    #[test]
    fn top_level_events_appear_in_background_activity_section() {
        let events = vec![
            ev(session_event_kind::INFO, "context compacted", None),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":7,"status":"Pending"}"#,
                None,
            ),
        ];
        let out = render(&[], &events, &default_meta(), true);
        assert!(
            out.contains("Background activity"),
            "top-level events must trigger the trailing section. got:\n{out}"
        );
        assert!(out.contains("context compacted"));
        assert!(
            out.contains("agent:7: Pending"),
            "BgTaskUpdate JSON must be pretty-printed in canonical agent:N form (#1158 e). got:\n{out}"
        );
    }

    #[test]
    fn iter_heartbeats_aggregated_into_summary_line_by_default() {
        // #1158 (d): per-iter Running heartbeats are noise in exports.
        // Default mode should drop them and emit ONE summary line per
        // task at the bottom. State transitions (Pending, Completed,
        // iter==0 "started" marker) must remain visible.
        let events = vec![
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":"Pending"}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":0}}}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":1}}}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":2}}}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":3}}}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Completed":{"summary":"done"}}}"#,
                None,
            ),
        ];
        let out = render(&[], &events, &default_meta(), true);
        // State transitions must survive.
        assert!(out.contains("agent:1: Pending"), "missing Pending: {out}");
        assert!(
            out.contains("\"iter\":0"),
            "iter==0 marker must survive (treated as state transition): {out}"
        );
        assert!(out.contains("Completed"), "Completed must survive: {out}");
        // The 3 iter>0 heartbeats must NOT appear individually.
        assert!(
            !out.contains("\"iter\":1"),
            "iter=1 heartbeat should be aggregated, not shown raw: {out}"
        );
        assert!(
            !out.contains("\"iter\":2"),
            "iter=2 heartbeat should be aggregated: {out}"
        );
        // Summary line must show the count of dropped heartbeats.
        assert!(
            out.contains("agent:1: 3 iterations aggregated"),
            "missing per-task summary line: {out}"
        );
    }

    #[test]
    fn verbose_mode_re_emits_every_iter_heartbeat() {
        // #1158 (d): KODA_EXPORT_VERBOSE=1 disables aggregation so
        // operators can debug bg-agent loops post-hoc.
        // SAFETY: tests in this module run sequentially per
        // `#[cfg(test)]` defaults; we restore the var before returning.
        // SAFETY (Rust 2024): set_var/remove_var are unsafe because
        // they mutate process-global env. Documented as fine in
        // single-threaded test contexts.
        // SAFETY: tests run sequentially; mutating process env in a
        // single-threaded test context is the documented use case.
        unsafe {
            std::env::set_var("KODA_EXPORT_VERBOSE", "1");
        }
        let events = vec![
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":1}}}"#,
                None,
            ),
            ev(
                session_event_kind::BG_TASK_UPDATE,
                r#"{"task_id":1,"status":{"Running":{"iter":2}}}"#,
                None,
            ),
        ];
        let out = render(&[], &events, &default_meta(), true);
        unsafe {
            std::env::remove_var("KODA_EXPORT_VERBOSE");
        }
        assert!(
            out.contains("\"iter\":1") && out.contains("\"iter\":2"),
            "verbose mode must preserve all heartbeats: {out}"
        );
        assert!(
            !out.contains("aggregated"),
            "verbose mode must NOT emit summary line: {out}"
        );
    }

    #[test]
    fn no_background_activity_section_when_no_top_level_events() {
        let out = render(&[], &[], &default_meta(), true);
        assert!(
            !out.contains("Background activity"),
            "empty events must not produce a noisy empty section"
        );
    }

    #[test]
    fn pretty_bg_task_update_falls_back_to_raw_on_bad_json() {
        // Defensive: malformed payloads must round-trip to the
        // transcript untouched, never silently dropped.
        let events = vec![ev(
            session_event_kind::BG_TASK_UPDATE,
            "not json at all {{{",
            None,
        )];
        let out = render(&[], &events, &default_meta(), true);
        assert!(
            out.contains("not json at all {{{"),
            "unparseable BgTaskUpdate must surface verbatim. got:\n{out}"
        );
    }
}
