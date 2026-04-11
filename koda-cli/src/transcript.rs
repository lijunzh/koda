//! Conversation transcript generator.
//!
//! Converts a session's `Message` slice into a human-readable Markdown
//! document suitable for clipboard copy or file export.
//!
//! ## Format
//!
//! ```text
//! # Koda Session — 2026-04-10 10:32
//!
//! ## User
//! What does this function do?
//!
//! ## Assistant
//! The function `foo()` does …
//!
//! ### 🔍 Read `src/main.rs`
//! > (3 lines shown)
//! > fn main() { … }
//!
//! ### ✏️ Edit `src/main.rs`
//! > Applied 1 change
//! ```
//!
//! Tool results are summarised (not dumped verbatim) to keep the export
//! concise. The `full_content` field is intentionally ignored \u2014 if users
//! want raw output they can re-run the tool.

use koda_core::persistence::{Message, Role};
use koda_core::tools::{ToolEffect, classify_tool};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Truncate a string to at most `max` characters, appending `…` if truncated.
/// Safe on multi-byte UTF-8 (never splits a codepoint).
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

/// Maximum content lines to include per tool result in the transcript.
const RESULT_PREVIEW_LINES: usize = 10;

/// Format a Unix timestamp as `YYYY-MM-DD HH:MM` (UTC).
///
/// Uses pure stdlib — no chrono dep needed.
fn format_utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple manual UTC decomposition (no DST, no timezone tables).
    let mins_total = secs / 60;
    let hours_total = mins_total / 60;
    let days_total = hours_total / 24;
    let min = mins_total % 60;
    let hour = hours_total % 24;
    // Gregorian calendar approximation for year/month/day.
    let (year, month, day) = gregorian_from_days(days_total as u32);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02} UTC")
}

/// Convert days-since-epoch to (year, month, day) in the proleptic Gregorian calendar.
fn gregorian_from_days(mut z: u32) -> (u32, u32, u32) {
    z += 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Generate a Markdown transcript from a slice of session messages.
///
/// Returns the transcript as a `String`. The caller is responsible for
/// writing it to the clipboard or a file.
pub fn render(messages: &[Message], session_title: Option<&str>) -> String {
    let mut out = String::with_capacity(4096);

    // Header
    let title = session_title.unwrap_or("Koda Session");
    let now = format_utc_now();
    out.push_str(&format!("# {title} — {now}\n\n"));

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
                out.push_str("---\n\n## \u{1f9d1} User\n\n");
                if let Some(ref content) = msg.content {
                    out.push_str(content.trim());
                    out.push_str("\n\n");
                }
            }

            Role::Assistant => {
                out.push_str("## 🤖 Assistant\n\n");

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
                    for call in &calls {
                        let name = call["function"]["name"].as_str().unwrap_or("Tool");
                        let args_json = call["function"]["arguments"].as_str().unwrap_or("{}");
                        let detail = tool_detail_summary(name, args_json);
                        let icon = tool_icon(name);
                        out.push_str(&format!("### {icon} **{name}**"));
                        if !detail.is_empty() {
                            out.push_str(&format!(" `{detail}`"));
                        }
                        out.push('\n');
                    }
                    out.push('\n');
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
                    // Only surface read-only tool output \u2014 write/bash output
                    // is usually noise (diffs, compiler output). Still summarise it.
                    match effect {
                        ToolEffect::ReadOnly => {
                            out.push_str("**Output:**\n\n```\n");
                            let preview_lines: Vec<&str> =
                                content.lines().take(RESULT_PREVIEW_LINES).collect();
                            out.push_str(&preview_lines.join("\n"));
                            if total_lines > RESULT_PREVIEW_LINES {
                                out.push_str(&format!(
                                    "\n… ({} more lines)",
                                    total_lines - RESULT_PREVIEW_LINES
                                ));
                            }
                            out.push_str("\n```\n\n");
                        }
                        _ => {
                            // Mutating tools: just note how many lines of output
                            if total_lines > 0 {
                                out.push_str(&format!(
                                    "> _{total_lines} line(s) of output — run tool to see full result_\n\n"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    out
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
fn tool_detail_summary(name: &str, args_json: &str) -> String {
    let args: serde_json::Value =
        serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
    match name {
        "Read" | "Write" | "Edit" | "Delete" => {
            args["file_path"].as_str().unwrap_or("").to_string()
        }
        "Bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.chars().count() > 80 {
                truncate_with_ellipsis(cmd, 77)
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

    #[test]
    fn empty_messages_produces_header_only() {
        let out = render(&[], Some("Test Session"));
        assert!(out.contains("# Test Session"));
        assert!(!out.contains("## 🧑 User"));
    }

    #[test]
    fn user_message_renders_correctly() {
        let msgs = vec![make_msg(Role::User, "hello koda")];
        let out = render(&msgs, None);
        assert!(out.contains("## 🧑 User"));
        assert!(out.contains("hello koda"));
    }

    #[test]
    fn assistant_message_renders_correctly() {
        let msgs = vec![make_msg(Role::Assistant, "I can help!")];
        let out = render(&msgs, None);
        assert!(out.contains("## 🤖 Assistant"));
        assert!(out.contains("I can help!"));
    }

    #[test]
    fn system_messages_skipped() {
        let msgs = vec![
            make_msg(Role::System, "secret prompt"),
            make_msg(Role::User, "hi"),
        ];
        let out = render(&msgs, None);
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
        let out = render(&msgs, None);
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
        let out = render(&msgs, None);
        // Bash is mutating → summarised, not shown verbatim
        assert!(!out.contains("line1"));
        assert!(out.contains("3 line(s) of output"));
    }
}
