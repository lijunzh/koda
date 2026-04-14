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

use koda_core::persistence::{Message, Role};
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

/// Truncate a string to at most `max` characters, appending `…` if truncated.
/// Safe on multi-byte UTF-8 (never splits a codepoint).
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

/// Maximum content lines to include per tool result in summary mode.
const SUMMARY_RESULT_LINES: usize = 10;

/// Maximum content lines to include per tool result in verbose mode.
const VERBOSE_RESULT_LINES: usize = 50;

/// Bash command truncation limit in summary mode.
const SUMMARY_BASH_CHARS: usize = 80;

/// Bash command truncation limit in verbose mode (effectively unlimited).
const VERBOSE_BASH_CHARS: usize = 500;

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

/// Generate a Markdown transcript from a slice of session messages.
///
/// - `verbose = true` (default): full fidelity — timestamps, token counts,
///   all tool output, session metadata header.
/// - `verbose = false` (`--summary`): concise, human-readable.
///
/// Returns the transcript as a `String`. The caller is responsible for
/// writing it to the clipboard or a file.
pub fn render(messages: &[Message], meta: &SessionMeta, verbose: bool) -> String {
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
                if let (Some(id), Some(name)) =
                    (call["id"].as_str(), call["function"]["name"].as_str())
                {
                    tool_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
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
                    for call in &calls {
                        let name = call["function"]["name"].as_str().unwrap_or("Tool");
                        let args_json = call["function"]["arguments"].as_str().unwrap_or("{}");
                        let detail = tool_detail_summary(name, args_json, bash_limit);
                        let icon = tool_icon(name);
                        out.push_str(&format!("### {icon} **{name}**"));
                        if !detail.is_empty() {
                            out.push_str(&format!(" `{detail}`"));
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
                        out.push_str("**Output:**\n\n```\n");
                        let preview_lines: Vec<&str> = content.lines().take(max_lines).collect();
                        out.push_str(&preview_lines.join("\n"));
                        if total_lines > max_lines {
                            out.push_str(&format!("\n… ({} more lines)", total_lines - max_lines));
                        }
                        out.push_str("\n```\n\n");
                    } else if total_lines > 0 {
                        out.push_str(&format!(
                            "> _{total_lines} line(s) of output — run tool to see full result_\n\n"
                        ));
                    }
                }
            }
        }
    }

    out
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
        "TodoWrite" | "TodoRead" => "📋",
        "MemoryWrite" | "MemoryRead" => "🧠",
        "InvokeAgent" => "🤖",
        "AskUser" => "💬",
        _ => "🔧",
    }
}

/// One-line summary of a tool call's arguments (mirrors `history_render`).
///
/// `bash_limit` controls Bash command truncation (80 in summary, 500 in verbose).
fn tool_detail_summary(name: &str, args_json: &str, bash_limit: usize) -> String {
    let args: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    match name {
        "Read" | "Write" | "Edit" | "Delete" => {
            args["file_path"].as_str().unwrap_or("").to_string()
        }
        "Bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.chars().count() > bash_limit {
                truncate_with_ellipsis(cmd, bash_limit.saturating_sub(3))
            } else {
                cmd.to_string()
            }
        }
        "Grep" => {
            let pat = args["search_string"]
                .as_str()
                .or_else(|| args["pattern"].as_str())
                .unwrap_or("");
            let dir = args["directory"].as_str().unwrap_or(".");
            format!("{pat} in {dir}")
        }
        "List" | "Glob" => args["directory"]
            .as_str()
            .or_else(|| args["path"].as_str())
            .unwrap_or(".")
            .to_string(),
        "WebFetch" => args["url"].as_str().unwrap_or("").to_string(),
        _ => {
            if let Some(obj) = args.as_object() {
                for (_, v) in obj.iter().take(1) {
                    if let Some(s) = v.as_str() {
                        return if s.chars().count() > 80 {
                            truncate_with_ellipsis(s, 77)
                        } else {
                            s.to_string()
                        };
                    }
                }
            }
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::persistence::Message;

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
        let out = render(&[], &meta, false);
        assert!(out.contains("# Test Session"));
        assert!(!out.contains("🧑 User"));
    }

    #[test]
    fn user_message_renders_correctly() {
        let msgs = vec![make_msg(Role::User, "hello koda")];
        let out = render(&msgs, &default_meta(), false);
        assert!(out.contains("🧑 User"));
        assert!(out.contains("hello koda"));
    }

    #[test]
    fn assistant_message_renders_correctly() {
        let msgs = vec![make_msg(Role::Assistant, "I can help!")];
        let out = render(&msgs, &default_meta(), false);
        assert!(out.contains("🤖 Assistant"));
        assert!(out.contains("I can help!"));
    }

    #[test]
    fn system_messages_skipped() {
        let msgs = vec![
            make_msg(Role::System, "secret prompt"),
            make_msg(Role::User, "hi"),
        ];
        let out = render(&msgs, &default_meta(), false);
        assert!(!out.contains("secret prompt"));
    }

    #[test]
    fn tool_read_result_shown_as_code_block() {
        let mut result_msg = make_msg(Role::Tool, "fn main() {}\n");
        result_msg.tool_call_id = Some("call_1".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(
            serde_json::json!([{
                "id": "call_1",
                "function": { "name": "Read", "arguments": r#"{"file_path":"src/main.rs"}"# }
            }])
            .to_string(),
        );

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &default_meta(), false);
        assert!(out.contains("```"));
        assert!(out.contains("fn main()"));
    }

    #[test]
    fn bash_result_shows_summary_not_content() {
        let mut result_msg = make_msg(Role::Tool, "line1\nline2\nline3");
        result_msg.tool_call_id = Some("call_2".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(
            serde_json::json!([{
                "id": "call_2",
                "function": { "name": "Bash", "arguments": r#"{"command":"ls"}"# }
            }])
            .to_string(),
        );

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &default_meta(), false);
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

        let out = render(&[msg], &default_meta(), false);
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
        let out = render(&[], &meta, true);
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

        let out = render(&[msg], &default_meta(), true);
        assert!(out.contains("100 prompt"), "prompt tokens shown");
        assert!(out.contains("50 completion"), "completion tokens shown");
        assert!(out.contains("80 cache-read"), "cache-read tokens shown");
    }

    #[test]
    fn summary_hides_token_counts() {
        let mut msg = make_msg(Role::Assistant, "The answer.");
        msg.prompt_tokens = Some(100);
        msg.completion_tokens = Some(50);

        let out = render(&[msg], &default_meta(), false);
        assert!(!out.contains("100 prompt"));
    }

    #[test]
    fn verbose_shows_timestamps() {
        let mut msg = make_msg(Role::User, "hello");
        msg.created_at = Some("2026-04-14T09:15:30Z".into());

        let out = render(&[msg], &default_meta(), true);
        assert!(out.contains("09:15:30"), "timestamp shown in verbose");
    }

    #[test]
    fn summary_hides_timestamps() {
        let mut msg = make_msg(Role::User, "hello");
        msg.created_at = Some("2026-04-14T09:15:30Z".into());

        let out = render(&[msg], &default_meta(), false);
        assert!(!out.contains("09:15:30"), "timestamp hidden in summary");
    }

    #[test]
    fn bash_result_shown_in_verbose_mode() {
        let mut result_msg = make_msg(Role::Tool, "line1\nline2\nline3");
        result_msg.tool_call_id = Some("call_3".into());

        let mut assistant_msg = make_msg(Role::Assistant, "");
        assistant_msg.tool_calls = Some(
            serde_json::json!([{
                "id": "call_3",
                "function": { "name": "Bash", "arguments": r#"{"command":"ls"}"# }
            }])
            .to_string(),
        );

        let msgs = vec![assistant_msg, result_msg];
        let out = render(&msgs, &default_meta(), true);
        // Verbose mode shows all tool output, including Bash
        assert!(out.contains("line1"));
        assert!(out.contains("line3"));
    }
}
