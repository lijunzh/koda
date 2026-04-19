//! Render historical DB messages into styled `Line`s for the scroll buffer.
//!
//! Used on session resume: loads prior conversation from the database and
//! renders it into the same visual format as live inference output.
//! Keeps the history view compact — tool results are summarized, not
//! replayed in full.

use std::collections::HashMap;

use koda_core::persistence::{Message, Role};
use koda_core::tools::{ToolEffect, classify_tool};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui_output::{BOLD, DIM, READ_CONTENT, TOOL_PREFIX, WRITE_CONTENT};

/// Maximum lines of tool output to show inline in history replay.
const TOOL_OUTPUT_PREVIEW_LINES: usize = 3;

/// Convert a slice of historical messages into styled `Line`s.
///
/// Renders user messages with a `❯` prompt, assistant text with a `───`
/// separator, tool calls as `● ToolName detail`, and tool results as
/// abbreviated summaries. Tool result styling is differentiated by tool type:
/// read-only tools (Read, Grep, List…) render their content in a readable
/// light color; mutating tools (Bash, Write, Edit…) stay dim.
pub fn render_history_messages(messages: &[Message]) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Build a tool_call_id → tool_name map so tool result messages can
    // look up which tool produced them and pick the right content style.
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
            Role::System => {
                // System prompt is internal — skip
            }
            Role::User => {
                render_user_message(&mut lines, msg);
            }
            Role::Assistant => {
                render_assistant_message(&mut lines, msg);
            }
            Role::Tool => {
                let tool_name = msg
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_id_to_name.get(id))
                    .map(|s| s.as_str())
                    .unwrap_or("");
                render_tool_result(&mut lines, msg, tool_name);
            }
        }
    }

    if !lines.is_empty() {
        // Visual separator between history and new conversation
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{2500}\u{2500}\u{2500} session resumed \u{2500}\u{2500}\u{2500}",
                DIM,
            ),
        ]));
        lines.push(Line::default());
    }

    lines
}

/// Render a user message: `  ❯ message text`
fn render_user_message(lines: &mut Vec<Line<'static>>, msg: &Message) {
    lines.push(Line::default());
    if let Some(ref content) = msg.content {
        // Show first line with prompt indicator, rest indented
        let mut iter = content.lines();
        if let Some(first) = iter.next() {
            lines.push(Line::from(vec![
                Span::styled(
                    "  \u{276f} ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(first.to_string(), BOLD),
            ]));
        }
        for rest in iter {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::raw(rest.to_string()),
            ]));
        }
    }
}

/// Render an assistant response: separator + text + any tool call headers.
fn render_assistant_message(lines: &mut Vec<Line<'static>>, msg: &Message) {
    // Response separator
    lines.push(Line::styled("  \u{2500}\u{2500}\u{2500}", DIM));

    // Thinking block — rendered before text, matching live streaming style:
    //   💭 Thinking...      ← header
    //   │ <line>            ← one line per newline in thinking_content
    if let Some(ref thinking) = msg.thinking_content
        && !thinking.is_empty()
    {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{1f4ad} Thinking", DIM),
        ]));
        for line in thinking.lines() {
            lines.push(Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(line.to_string(), DIM),
            ]));
        }
    }

    // Text content (markdown rendered as plain styled text for history)
    if let Some(ref content) = msg.content
        && !content.is_empty()
    {
        for line in content.lines() {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(line.to_string()),
            ]));
        }
    }

    // Tool calls (if any) — show as headers
    if let Some(ref tc_json) = msg.tool_calls {
        render_tool_call_headers(lines, tc_json);
    }
}

/// Parse tool_calls JSON and render `● ToolName <styled detail>` headers.
///
/// Detail formatting and colors are delegated to
/// [`crate::tool_header::build_header_line_from_str`] so live render
/// and history replay produce identical span sequences for the same
/// `(name, args)` input.
fn render_tool_call_headers(lines: &mut Vec<Line<'static>>, tc_json: &str) {
    // Tool calls are stored as JSON arrays (OpenAI format)
    let calls: Vec<serde_json::Value> = match serde_json::from_str(tc_json) {
        Ok(v) => v,
        Err(_) => return,
    };

    for call in &calls {
        let name = call["function"]["name"].as_str().unwrap_or("unknown");
        let args = call["function"]["arguments"].as_str().unwrap_or("{}");
        lines.push(crate::tool_header::build_header_line_from_str(
            "", name, args,
        ));
    }
}

/// Render a tool result message (abbreviated).
///
/// Content style is determined by the tool type:
/// - Read-only tools (Read, Grep, List, Glob…) → `READ_CONTENT` (legible light gray)
/// - Mutating tools (Bash, Write, Edit…)        → `WRITE_CONTENT` (dim, less important)
fn render_tool_result(lines: &mut Vec<Line<'static>>, msg: &Message, tool_name: &str) {
    let content = msg.content.as_deref().unwrap_or("");
    let total_lines = content.lines().count();

    let content_style = match classify_tool(tool_name) {
        ToolEffect::ReadOnly => READ_CONTENT,
        _ => WRITE_CONTENT,
    };

    if total_lines == 0 {
        lines.push(Line::from(vec![
            Span::styled("  \u{2514} ", TOOL_PREFIX),
            Span::styled("(empty)", DIM),
        ]));
        return;
    }

    if total_lines <= TOOL_OUTPUT_PREVIEW_LINES {
        // Short output — show in full
        for line in content.lines() {
            lines.push(Line::from(vec![
                Span::styled("  \u{2502} ", TOOL_PREFIX),
                Span::styled(line.to_string(), content_style),
            ]));
        }
    } else {
        // Long output — show preview + count
        for line in content.lines().take(TOOL_OUTPUT_PREVIEW_LINES) {
            lines.push(Line::from(vec![
                Span::styled("  \u{2502} ", TOOL_PREFIX),
                Span::styled(line.to_string(), content_style),
            ]));
        }
        let hidden = total_lines - TOOL_OUTPUT_PREVIEW_LINES;
        lines.push(Line::from(vec![
            Span::styled("  \u{2514} ", TOOL_PREFIX),
            Span::styled(format!("... {hidden} more line(s)"), DIM),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
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
    fn test_empty_messages() {
        let lines = render_history_messages(&[]);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_user_message_rendering() {
        let messages = vec![msg(Role::User, "hello world")];
        let lines = render_history_messages(&messages);
        // Should have: blank line + prompt line + separator lines
        assert!(lines.len() >= 2);
        let prompt_line = &lines[1];
        let text: String = prompt_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("hello world"));
        assert!(text.contains('\u{276f}'));
    }

    #[test]
    fn test_assistant_message_rendering() {
        let messages = vec![msg(Role::User, "hello"), msg(Role::Assistant, "Hi there!")];
        let lines = render_history_messages(&messages);
        // Should contain the assistant separator and text
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("Hi there!"));
        assert!(all_text.contains("\u{2500}\u{2500}\u{2500}"));
    }

    #[test]
    fn test_tool_result_short() {
        let messages = vec![msg(Role::Tool, "line 1\nline 2")];
        let lines = render_history_messages(&messages);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("line 1"));
        assert!(all_text.contains("line 2"));
        // No "more lines" summary for short output
        assert!(!all_text.contains("more line"));
    }

    #[test]
    fn test_tool_result_long_truncated() {
        let long_output: String = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let messages = vec![msg(Role::Tool, &long_output)];
        let lines = render_history_messages(&messages);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("line 0"));
        assert!(all_text.contains("more line"));
    }

    #[test]
    fn test_system_messages_skipped() {
        let messages = vec![
            msg(Role::System, "You are a helpful assistant"),
            msg(Role::User, "hello"),
        ];
        let lines = render_history_messages(&messages);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(!all_text.contains("helpful assistant"));
    }

    #[test]
    fn test_tool_detail_summary() {
        // The detail logic now lives in `tool_header` and is exhaustively
        // covered there; this test just pins the *integration* — history
        // replay must produce the same colored spans as live render does.
        let typed = crate::tool_header::build_header_line(
            "",
            "Grep",
            &serde_json::json!({"search_string": "foo", "directory": "src"}),
        );
        let history = crate::tool_header::build_header_line_from_str(
            "",
            "Grep",
            r#"{"search_string": "foo", "directory": "src"}"#,
        );
        let typed_text: String = typed.spans.iter().map(|s| s.content.as_ref()).collect();
        let history_text: String = history.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(typed_text, history_text);
        assert!(history_text.contains("\"foo\""));
        assert!(history_text.contains("src"));
    }

    #[test]
    fn test_session_resumed_separator() {
        let messages = vec![msg(Role::User, "hello")];
        let lines = render_history_messages(&messages);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("session resumed"));
    }
}
