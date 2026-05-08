//! Render historical DB messages into styled `Line`s for the scroll buffer.
//!
//! Used on session resume: loads prior conversation from the database and
//! renders it into the same visual format as live inference output.
//! Keeps the history view compact — tool results are summarized, not
//! replayed in full.

use std::collections::HashMap;

use koda_core::persistence::{Message, Role, SessionEvent};
use koda_core::tools::{ToolCatalog, ToolEffect};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui_output::{BOLD, DIM, READ_CONTENT, TOOL_PREFIX, WRITE_CONTENT};

/// Maximum lines of tool output to show inline in history replay.
const TOOL_OUTPUT_PREVIEW_LINES: usize = 3;

/// Convert a slice of historical messages into styled `Line`s.
///
/// Back-compat shim: renders messages without folding any sub-agent
/// activity. Prefer [`render_history`] for new call sites — it
/// additionally folds persisted `SessionEvent`s under their parent
/// `InvokeAgent` tool call so resumed transcripts and debug bundles
/// can show what each sub-agent did. Kept as a thin wrapper so the
/// existing test suite (and any external callers) doesn't churn.
pub fn render_history_messages(messages: &[Message]) -> Vec<Line<'static>> {
    render_history(messages, &[])
}

/// Convert historical messages **plus** persisted sub-agent events
/// into styled `Line`s.
///
/// Sub-agent activity is persisted as `SessionEvent` rows of kind
/// `sub_agent_event` keyed by the spawning `InvokeAgent` call's
/// `tool_call_id` (#1108 P2a). This renderer folds those events
/// inline under the corresponding parent tool result so the
/// transcript reader can see what the sub-agent actually did instead
/// of just "Background agent 'X' started" followed by a black box
/// (#1344 issue A).
///
/// Visual treatment is gemini-cli inspired: an indented activity
/// block opened with a `┌── X (sub-agent activity) ──` header,
/// the pre-formatted event lines (which already carry their own
/// emoji prefix from `BufferingSink::classify_for_persist`), and
/// a closing `└──` bar. Distinct enough from the parent's `│`
/// gutter that eye-sweeps can tell whose activity each line
/// belongs to.
///
/// Renders user messages with a `❯` prompt, assistant text with a `───`
/// separator, tool calls as `● ToolName detail`, and tool results as
/// abbreviated summaries. Tool result styling is differentiated by tool type:
/// read-only tools (Read, Grep, List…) render their content in a readable
/// light color; mutating tools (Bash, Write, Edit…) stay dim.
pub fn render_history(messages: &[Message], events: &[SessionEvent]) -> Vec<Line<'static>> {
    // Index sub-agent events by their parent InvokeAgent tool_call_id
    // so we can fold them in O(1) when we hit the matching tool
    // result during the message walk. Empty when no sub-agents ran
    // — the inner loop just won't find any matches and renders
    // exactly as before. Order within each call_id is preserved
    // (events are loaded ASC by id from `load_session_events`), so
    // sub-agent activity appears in chronological emit order.
    let mut events_by_parent: HashMap<&str, Vec<&SessionEvent>> = HashMap::new();
    use koda_core::persistence::session_event_kind as sek;
    for ev in events {
        if ev.kind != sek::SUB_AGENT_EVENT {
            // Other event kinds (Info, BgTaskUpdate) aren't part of
            // this fold — they belong to the parent's own narrative
            // and are out of scope for #1344-A. Future work could
            // surface them as their own row type.
            continue;
        }
        if let Some(parent) = ev.parent_tool_call_id.as_deref() {
            events_by_parent.entry(parent).or_default().push(ev);
        }
    }

    let mut lines: Vec<Line<'static>> = Vec::new();

    // tool_call_id → tool_name mapping for result correlation, populated
    // **incrementally during the render walk below** rather than via a
    // pre-pass over all messages. Same bug class as #1164 fixed in
    // `transcript.rs`: providers like Gemini emit per-turn tool_call_ids
    // (`gemini_tc_1`, `gemini_tc_2`, …) that reset every assistant
    // message, so a global last-write-wins pre-pass would silently
    // overwrite an earlier turn's `WaitTask` mapping with a later turn's
    // `Read`, causing the resumed-history WaitTask result to skip the
    // pretty-printer and dump raw JSON.
    //
    // Walking in order and inserting as we go means each Tool message
    // sees the rolling map state **as of that point in the transcript**,
    // which always reflects the most-recent prior Assistant tool_calls
    // block — i.e. the call that actually produced this result.
    let mut tool_id_to_name: HashMap<String, String> = HashMap::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                // System prompt is internal — skip
            }
            Role::User => {
                render_user_message(&mut lines, msg);
            }
            Role::Assistant => {
                if let Some(ref tc_json) = msg.tool_calls
                    && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
                {
                    for call in calls {
                        if let (Some(id), Some(name)) =
                            (call["id"].as_str(), tool_call_field(&call, "name"))
                        {
                            tool_id_to_name.insert(id.to_string(), name.to_string());
                        }
                    }
                }
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

                // #1344 issue A: fold persisted sub-agent events under
                // the parent's `InvokeAgent` tool result. Pre-this the
                // resumed transcript / debug bundle showed the spawn
                // line and a black box — the trace lines lived in the
                // DB (since #1108 P2a) but no renderer consumed them.
                // Restricted to `InvokeAgent` for now: that's the only
                // tool whose results have correlated sub-agent events.
                // Other tools fall through unchanged.
                if tool_name == "InvokeAgent"
                    && let Some(call_id) = msg.tool_call_id.as_deref()
                    && let Some(child_events) = events_by_parent.get(call_id)
                    && !child_events.is_empty()
                {
                    render_sub_agent_activity(&mut lines, child_events);
                }
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
    // Tool calls are persisted as `serde_json::to_string(&Vec<ToolCall>)`
    // — see `koda_core::providers::ToolCall`. The struct serializes flat:
    // `{"id":"…","function_name":"…","arguments":"…"}`. Pre-#1340 this
    // function read `call["function"]["name"]` (the OpenAI wire shape
    // we never persist), so every header rendered as `● unknown` in
    // resumed-session history and debug-bundle replays — the bug filed
    // as R7 of #1324. The `tool_call_field` helper accepts both shapes
    // for forward-compat with any external tooling that hand-builds the
    // legacy shape, but the canonical shape is the flat one.
    let calls: Vec<serde_json::Value> = match serde_json::from_str(tc_json) {
        Ok(v) => v,
        Err(_) => return,
    };

    for call in &calls {
        let name = tool_call_field(call, "name").unwrap_or("unknown");
        let args = tool_call_field(call, "arguments").unwrap_or("{}");
        lines.push(crate::tool_header::build_header_line_from_str(
            "", name, args,
        ));
    }
}

/// Read a logical tool-call field from either persistence shape.
///
/// Production code persists [`koda_core::providers::ToolCall`] flat
/// (`{"function_name":"…","arguments":"…"}`), but for forward-compat
/// we also accept the OpenAI wire shape (`{"function":{"name":"…",
/// "arguments":"…"}}`) so external bundles or hand-crafted history
/// files render correctly too.
///
/// `field` is the *logical* name: `"name"` or `"arguments"`.
fn tool_call_field<'a>(call: &'a serde_json::Value, field: &str) -> Option<&'a str> {
    let canonical = match field {
        "name" => "function_name",
        "arguments" => "arguments",
        _ => field,
    };
    call.get(canonical).and_then(|v| v.as_str()).or_else(|| {
        call.get("function")
            .and_then(|f| f.get(field))
            .and_then(|v| v.as_str())
    })
}

/// Render a tool result message (abbreviated).
///
/// Content style is determined by the tool type:
/// - Read-only tools (Read, Grep, List, Glob…) → `READ_CONTENT` (legible light gray)
/// - Mutating tools (Bash, Write, Edit…)        → `WRITE_CONTENT` (dim, less important)
fn render_tool_result(lines: &mut Vec<Line<'static>>, msg: &Message, tool_name: &str) {
    let content = msg.content.as_deref().unwrap_or("");

    // WaitTask returns aggregated multi-task JSON (#1157) that's
    // user-hostile when dumped raw. Pretty-print it as a per-task
    // summary instead, mirroring the live streaming render
    // (`tui_render::render_tool_output`) and the markdown export
    // (`transcript::pretty_wait_task_output`). Falls back to the
    // generic line-by-line path on any parse failure so we never
    // lose the raw content.
    if tool_name == "WaitTask"
        && let Some(rendered) = crate::wait_task_format::try_render_wait_task_lines(content)
    {
        lines.extend(rendered);
        return;
    }

    // ListBackgroundTasks (#1209) — same JSON-soup problem; render
    // with the same per-task helper so resumed history matches the
    // live `tui_render` output verbatim.
    if tool_name == "ListBackgroundTasks"
        && let Some(rendered) = crate::wait_task_format::try_render_list_bg_tasks_lines(content)
    {
        lines.extend(rendered);
        return;
    }

    // WaitForMail (#1336, enriched in #1343) returns a structured
    // JSON envelope that's borderline unreadable when dumped raw —
    // particularly the timeout case with the rich `bg_agents` array
    // and `hint` text. Pretty-print it as a header + per-agent rows
    // + hint line, matching the per-task style of the two siblings
    // above so the three tools share one visual vocabulary.
    // Same fail-safe: fall through to the generic render on any
    // shape mismatch (#1344 issue B).
    if tool_name == "WaitForMail"
        && let Some(rendered) = crate::wait_task_format::try_render_wait_for_mail_lines(content)
    {
        lines.extend(rendered);
        return;
    }

    let total_lines = content.lines().count();

    let content_style =
        match ToolCatalog::default_static().classify_call(tool_name, &serde_json::Value::Null) {
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

/// Render persisted sub-agent activity events as an indented block
/// under the parent's `InvokeAgent` tool result.
///
/// Visual shape — gemini-cli inspired but plain-text-friendly so it
/// survives `lines_to_text` flattening into `conversation.md`:
///
/// ```text
///   │ ┌── sub-agent activity ──
///   │   🔧 List
///   │   🔧 Read koda-core/src/lib.rs
///   │   🔧 Grep
///   │ └──
/// ```
///
/// Each event line is already pre-formatted with its own emoji prefix
/// (`🔧` tool, `⎘` auto-rejected approval, raw text for Info) by
/// `BufferingSink::classify_for_persist`. We just box it.
///
/// Header intentionally generic: a future iteration can derive the
/// agent name from the spawn arguments. The activity block itself is
/// the visual win — distinguishing parent from sub-agent work —
/// and that doesn't depend on knowing the name.
fn render_sub_agent_activity(lines: &mut Vec<Line<'static>>, events: &[&SessionEvent]) {
    // Opening frame.
    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", TOOL_PREFIX),
        Span::styled(
            "\u{250c}\u{2500}\u{2500} sub-agent activity \u{2500}\u{2500}".to_string(),
            DIM,
        ),
    ]));

    // Event lines. Each event payload already starts with its own
    // 2-space indent + emoji (`  \u{1f527} ToolName`) so we just
    // prepend the gutter prefix and ship it.
    for ev in events {
        lines.push(Line::from(vec![
            Span::styled("  \u{2502} ", TOOL_PREFIX),
            Span::styled(ev.payload.clone(), READ_CONTENT),
        ]));
    }

    // Closing frame.
    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", TOOL_PREFIX),
        Span::styled("\u{2514}\u{2500}\u{2500}".to_string(), DIM),
    ]));
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

    /// Helper: build an Assistant message with a synthetic tool_calls
    /// JSON declaring one tool call. Uses the canonical persistence
    /// shape (`function_name` flat) — see [`koda_core::providers::ToolCall`].
    fn assistant_calling(name: &str, call_id: &str) -> Message {
        let calls = serde_json::json!([{
            "id": call_id,
            "function_name": name,
            "arguments": "{}"
        }]);
        let mut m = msg(Role::Assistant, "");
        m.tool_calls = Some(calls.to_string());
        m
    }

    /// Helper: build a Tool result message tagged with `tool_call_id`.
    fn tool_result(call_id: &str, content: &str) -> Message {
        let mut m = msg(Role::Tool, content);
        m.tool_call_id = Some(call_id.into());
        m
    }

    #[test]
    fn wait_task_result_renders_as_per_task_summary_not_raw_json() {
        // The bug from session koda-20260430-152051: WaitTask results
        // dumped raw JSON in the live TUI / resumed history because
        // neither surface was WaitTask-aware (the pretty-printer existed
        // only in `transcript.rs`). After this fix, both TUI surfaces
        // share the same per-task summary via `wait_task_format`.
        let payload = serde_json::json!({
            "summary": {"total": 2, "completed": 2},
            "tasks": [
                {"task_id": "agent:1", "status": "completed", "agent_name": "explore",
                 "output": "Scan finished. Found 0 issues."},
                {"task_id": "agent:2", "status": "completed", "agent_name": "explore",
                 "output": "All providers verified."},
            ],
        });
        let messages = vec![
            assistant_calling("WaitTask", "c1"),
            tool_result("c1", &payload.to_string()),
        ];
        let lines = render_history_messages(&messages);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();

        // Per-task structure must surface, not the raw JSON soup.
        assert!(
            all.contains("2 task(s) gathered")
                && all.contains("agent:1")
                && all.contains("agent:2"),
            "per-task summary missing: {all}"
        );
        // Critically: the raw JSON envelope keys must NOT appear as
        // visible text — that's the bug we're fixing.
        assert!(
            !all.contains("\"summary\":") && !all.contains("\"tasks\":"),
            "raw JSON keys leaked into rendered output: {all}"
        );
        // The agent's actual content surfaces in the preview.
        assert!(
            all.contains("Scan finished"),
            "task 1 preview missing: {all}"
        );
    }

    #[test]
    fn wait_task_pretty_survives_gemini_style_tool_id_reuse_across_turns() {
        // Same id-collision bug class fixed in `transcript.rs` via #1164,
        // applied to the resumed-history path. Gemini emits per-turn
        // tool_call_ids (`gemini_tc_1`, …) that reset every assistant
        // message; the old global pre-pass in `render_history_messages`
        // would let a later `Read` overwrite an earlier `WaitTask`
        // mapping, causing the WaitTask result to render as raw JSON
        // even though the pretty-printer was wired in.
        //
        // Shape (mirrors the user-reported pattern):
        //   T1: assistant calls WaitTask  (id=tc_1) → WaitTask JSON result
        //   T2: assistant calls Read      (id=tc_1, REUSED) → file body
        let payload = serde_json::json!({
            "summary": {"total": 1, "completed": 1},
            "tasks": [{
                "task_id": "agent:1",
                "status": "completed",
                "agent_name": "explore",
                "output": "All clear.",
            }],
        });
        let messages = vec![
            assistant_calling("WaitTask", "tc_1"),
            tool_result("tc_1", &payload.to_string()),
            assistant_calling("Read", "tc_1"),
            tool_result("tc_1", "fn main() {}\n"),
        ];
        let lines = render_history_messages(&messages);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();

        // WaitTask must still pretty-print despite the later Read reusing
        // the same id.
        assert!(
            all.contains("task(s) gathered") && all.contains("agent:1"),
            "WaitTask pretty render missing despite later id reuse: {all}"
        );
        assert!(
            !all.contains("\"summary\":"),
            "raw JSON leaked — id-collision bug regressed: {all}"
        );
        // Sanity: the later Read result still renders normally.
        assert!(
            all.contains("fn main()"),
            "Read result must still render: {all}"
        );
    }

    /// R7 of #1324: pre-#1340, `render_tool_call_headers` read tool
    /// calls assuming the OpenAI wire shape (`call["function"]["name"]`)
    /// while production persisted them flat (`call["function_name"]`).
    /// Every tool call in resumed history / debug-bundle replay
    /// rendered as `● unknown`. This test serializes a real `ToolCall`
    /// (the same path production uses) and asserts the rendered name
    /// is the actual tool name, not `"unknown"`.
    #[test]
    fn render_uses_canonical_tool_call_persistence_shape() {
        use koda_core::providers::ToolCall;

        let tcs = vec![ToolCall {
            id: "call_abc".into(),
            function_name: "WebFetch".into(),
            arguments: r#"{"url":"https://example.com"}"#.into(),
            thought_signature: None,
        }];
        let tc_json = serde_json::to_string(&tcs).expect("ToolCall must serialize");

        let mut assistant = msg(Role::Assistant, "");
        assistant.tool_calls = Some(tc_json);

        let lines = render_history_messages(&[assistant]);
        let all: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all.contains("WebFetch"),
            "tool name must render from canonical shape; got: {all}"
        );
        assert!(
            !all.contains("unknown"),
            "the '\u{25cf} unknown' bug from #1324 R7 must not regress; got: {all}"
        );
    }

    /// Forward-compat: external bundles may use the OpenAI wire shape
    /// (`{"function":{"name":"…"}}`). The renderer accepts both shapes
    /// so hand-crafted history files don't render as `● unknown` either.
    #[test]
    fn render_also_accepts_legacy_openai_wire_shape() {
        let calls = serde_json::json!([{
            "id": "call_xyz",
            "function": {"name": "Grep", "arguments": "{}"}
        }]);
        let mut assistant = msg(Role::Assistant, "");
        assistant.tool_calls = Some(calls.to_string());

        let lines = render_history_messages(&[assistant]);
        let all: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all.contains("Grep"),
            "OpenAI shape must also work; got: {all}"
        );
        assert!(!all.contains("unknown"), "got: {all}");
    }

    // --- #1344 issue A: sub-agent activity folding -------------------------

    /// Build an `InvokeAgent` assistant message + tool result pair
    /// with a stable `tool_call_id` so events can be correlated.
    /// Returns the (assistant_msg, tool_result_msg) pair plus the
    /// `tool_call_id` for use in event fixtures.
    fn invoke_agent_pair(call_id: &str, agent_name: &str) -> (Message, Message, String) {
        let assistant = Message {
            tool_calls: Some(format!(
                r#"[{{"id":"{call_id}","function":{{"name":"InvokeAgent","arguments":"{{\"agent\":\"{agent_name}\"}}"}}}}]"#
            )),
            ..msg(Role::Assistant, "")
        };
        let tool_result = Message {
            tool_call_id: Some(call_id.to_string()),
            ..msg(
                Role::Tool,
                &format!("Background agent '{agent_name}' started (agent:1)."),
            )
        };
        (assistant, tool_result, call_id.to_string())
    }

    fn sub_agent_event(parent_call_id: &str, payload: &str) -> SessionEvent {
        use koda_core::persistence::session_event_kind as sek;
        SessionEvent {
            id: 0,
            session_id: "test".into(),
            kind: sek::SUB_AGENT_EVENT.to_string(),
            payload: payload.to_string(),
            parent_tool_call_id: Some(parent_call_id.to_string()),
            created_at: None,
        }
    }

    fn collect_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn sub_agent_events_fold_under_invoke_agent_result() {
        let (assistant, tool_result, call_id) = invoke_agent_pair("call_1", "explore");
        let messages = vec![assistant, tool_result];
        let events = vec![
            sub_agent_event(&call_id, "  \u{1f527} List"),
            sub_agent_event(&call_id, "  \u{1f527} Read koda-core/src/lib.rs"),
            sub_agent_event(&call_id, "  \u{1f527} Grep"),
        ];
        let lines = render_history(&messages, &events);
        let all = collect_text(&lines);

        // Opening + closing frame markers.
        assert!(
            all.contains("sub-agent activity"),
            "missing activity block header: {all}"
        );
        assert!(
            all.contains("\u{250c}\u{2500}\u{2500}"),
            "missing opening corner: {all}"
        );
        assert!(
            all.contains("\u{2514}\u{2500}\u{2500}"),
            "missing closing corner: {all}"
        );

        // Each event payload appears verbatim with its preformatted prefix.
        assert!(all.contains("\u{1f527} List"), "missing List event: {all}");
        assert!(
            all.contains("\u{1f527} Read koda-core/src/lib.rs"),
            "missing Read event: {all}"
        );
        assert!(all.contains("\u{1f527} Grep"), "missing Grep event: {all}");
    }

    #[test]
    fn no_events_means_no_activity_block() {
        // Same shape as the fold test but with empty events. The
        // InvokeAgent result still renders, but the activity frame
        // must not appear — a black-box experience for sessions
        // pre-#1108 or sessions where no sub-agents ran.
        let (assistant, tool_result, _) = invoke_agent_pair("call_1", "explore");
        let messages = vec![assistant, tool_result];
        let lines = render_history(&messages, &[]);
        let all = collect_text(&lines);
        assert!(
            !all.contains("sub-agent activity"),
            "unexpected activity block: {all}"
        );
    }

    #[test]
    fn events_only_fold_under_their_own_invoke_agent_call() {
        // Two InvokeAgent calls in the transcript; events keyed to
        // call_1 must NOT bleed into call_2's render. Guards against
        // a global last-write-wins bug similar to the one that
        // motivated the per-walk tool_id_to_name map.
        let (a1, t1, _) = invoke_agent_pair("call_1", "explore");
        let (a2, t2, _) = invoke_agent_pair("call_2", "verify");
        let messages = vec![a1, t1, a2, t2];
        let events = vec![sub_agent_event("call_1", "  \u{1f527} ExploreOnly")];
        let lines = render_history(&messages, &events);
        let all = collect_text(&lines);

        // ExploreOnly must appear once — under call_1's result, not call_2's.
        let occurrences = all.matches("ExploreOnly").count();
        assert_eq!(occurrences, 1, "event should fold once only: {all}");

        // Locate ExploreOnly relative to the two spawn results to
        // confirm placement: it should appear *before* the second
        // "started (agent:1)" line, i.e. inside call_1's block.
        let explore_idx = all.find("ExploreOnly").expect("event present");
        let second_started_idx = all
            .match_indices("started (agent:1)")
            .nth(1)
            .map(|(i, _)| i)
            .expect("two spawn results expected");
        assert!(
            explore_idx < second_started_idx,
            "event should fold under call_1, not call_2: {all}"
        );
    }

    #[test]
    fn non_invoke_agent_tool_results_are_unaffected() {
        // A `Read` tool result with sub-agent events keyed to its id
        // (which would never happen in practice — only InvokeAgent
        // produces sub-agent events) must NOT trigger the fold,
        // because we gate on `tool_name == "InvokeAgent"`. Belt-and-
        // suspenders test for the gate.
        let assistant = Message {
            tool_calls: Some(
                r#"[{"id":"call_1","function":{"name":"Read","arguments":"{}"}}]"#.to_string(),
            ),
            ..msg(Role::Assistant, "")
        };
        let tool_result = Message {
            tool_call_id: Some("call_1".to_string()),
            ..msg(Role::Tool, "file contents...")
        };
        let messages = vec![assistant, tool_result];
        let events = vec![sub_agent_event("call_1", "  \u{1f527} Should not appear")];
        let lines = render_history(&messages, &events);
        let all = collect_text(&lines);
        assert!(
            !all.contains("sub-agent activity"),
            "non-InvokeAgent tool must not fold events: {all}"
        );
        assert!(
            !all.contains("Should not appear"),
            "event payload must not leak: {all}"
        );
    }

    #[test]
    fn non_sub_agent_event_kinds_are_ignored() {
        // The events vec carries multiple kinds (Info, BgTaskUpdate,
        // SubAgentEvent). Only sub_agent_event entries should fold;
        // other kinds are out of scope for #1344-A. Guards against
        // an over-eager match dropping every persisted event into
        // every InvokeAgent block.
        use koda_core::persistence::session_event_kind as sek;
        let (assistant, tool_result, call_id) = invoke_agent_pair("call_1", "explore");
        let messages = vec![assistant, tool_result];
        let events = vec![
            // Wrong kind — must be ignored.
            SessionEvent {
                id: 0,
                session_id: "test".into(),
                kind: sek::INFO.to_string(),
                payload: "top-level info, not folded".to_string(),
                parent_tool_call_id: Some(call_id.clone()),
                created_at: None,
            },
            // Right kind — must fold.
            sub_agent_event(&call_id, "  \u{1f527} RealEvent"),
        ];
        let lines = render_history(&messages, &events);
        let all = collect_text(&lines);
        assert!(
            all.contains("RealEvent"),
            "sub_agent_event must fold: {all}"
        );
        assert!(
            !all.contains("top-level info, not folded"),
            "info events must not leak into activity block: {all}"
        );
    }

    #[test]
    fn render_history_messages_remains_back_compat() {
        // The shim must keep working with no events — same output
        // as render_history(msgs, &[]).
        let (assistant, tool_result, _) = invoke_agent_pair("call_1", "explore");
        let messages = vec![assistant, tool_result];
        let via_shim = render_history_messages(&messages);
        let via_new = render_history(&messages, &[]);
        let shim_text = collect_text(&via_shim);
        let new_text = collect_text(&via_new);
        assert_eq!(shim_text, new_text, "shim and new fn must agree");
    }
}
