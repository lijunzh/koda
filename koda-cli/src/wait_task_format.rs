//! Shared formatting primitives for `WaitTask` aggregated results.
//!
//! `WaitTask` (#1157) returns a JSON envelope with N sub-task results
//! (`{tasks: [...], summary: {total, completed, ...}}`). Two rendering
//! surfaces consume this same payload:
//!
//!   1. Live TUI streaming ([`crate::tui_render`]) — ratatui `Span`/`Line`
//!   2. Resumed history TUI ([`crate::history_render`]) — same as #1
//!
//! (A third surface — markdown export — lived in `transcript.rs`
//! until RFC #1167 PR δ collapsed `/export` into `/debug-bundle`.
//! `/debug-bundle` consumes `history_render` directly so this module's
//! primitives still feed exported text via the TUI → lines_to_text
//! pipeline.)
//!
//! This module owns the JSON-shape parsing and presentation primitives
//! (icons, previews) shared by both surfaces. Each surface owns the
//! styling appropriate to its medium.
//!
//! Centralising the icon table and preview helper means a status-set
//! change (e.g. adding `parse_error`) lights up consistently across all
//! renderers in one edit.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui_output::{BOLD, DIM, TOOL_PREFIX};

/// Per-task status icon. Centralised so the summary line, per-task
/// header, and resumed-history render can't drift apart. New statuses
/// added to the engine must land here as well — the catch-all `❔`
/// makes the omission visible in rendered output rather than failing
/// silently.
///
/// Covers the union of statuses emitted by **both** `WaitTask` (the
/// original consumer) and `ListBackgroundTasks` (#1209 added the latter):
///
/// - `WaitTask` outcomes: `completed`, `timed_out`, `cancelled`,
///   `not_found`, `forbidden`, `parse_error`.
/// - `ListBackgroundTasks` agent statuses: `pending`, `running`,
///   `completed`, `cancelled`, `errored`.
/// - `ListBackgroundTasks` process statuses: `running`, `exited`,
///   `killed`.
#[must_use]
pub fn wait_status_icon(status: &str) -> &'static str {
    match status {
        "completed" => "\u{2705}",   // ✅
        "timed_out" => "\u{23F1}",   // ⏱
        "cancelled" => "\u{26D4}",   // ⛔
        "not_found" => "\u{2753}",   // ❓
        "forbidden" => "\u{1F512}",  // 🔒
        "parse_error" => "\u{26A0}", // ⚠
        // ListBackgroundTasks-specific statuses (#1209). Glyph choice
        // matches `bg_activity_overlay`'s status palette so the live
        // overlay and the tool-result render speak the same icon
        // vocabulary.
        "pending" => "\u{25D0}", // ◐ — half-filled circle
        "running" => "\u{25B6}", // ▶ — play / in-flight
        "errored" => "\u{2717}", // ✗ — cross
        "exited" => "\u{2713}",  // ✓ — check (clean exit)
        "killed" => "\u{26D4}",  // ⛔ — same as cancelled (also user-initiated stop)
        _ => "\u{2754}",         // ❔ — catch-all (visible omission)
    }
}

/// One-line preview of an agent's output for the collapsed summary
/// line. Strips markdown noise (heading hashes, blockquote markers),
/// collapses interior whitespace, truncates with an ellipsis.
///
/// 120 chars is the markdown export's preferred width; the TUI uses
/// shorter limits because terminal columns are scarcer than browser
/// width. Chars (not bytes) so multibyte content isn't truncated mid
/// codepoint.
#[must_use]
pub fn first_meaningful_line(output: &str, max_chars: usize) -> String {
    for raw in output.lines() {
        let trimmed = raw
            .trim()
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim();
        if trimmed.is_empty() {
            continue;
        }
        let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.chars().count() <= max_chars {
            return collapsed;
        }
        let truncated: String = collapsed.chars().take(max_chars).collect();
        return format!("{truncated}\u{2026}");
    }
    String::new()
}

/// Per-task TUI preview width. Shorter than the markdown export's 120
/// because terminal columns are shared with the `│` prefix, status
/// icon, task id, and agent name — anything longer wraps awkwardly.
const TUI_PREVIEW_CHARS: usize = 80;

/// Render a `WaitTask` JSON payload as styled TUI lines, or `None` on
/// any parse failure or shape mismatch. Caller falls back to the
/// generic tool-output render path on `None` — we never want to lose
/// the raw content when the pretty path can't run.
///
/// Output shape (one `│` prefix per line, matching the surrounding
/// `render_tool_output` convention):
///
/// ```text
///   │ 4 task(s) gathered — ✅ 4 completed
///   │   ✅ agent:1  explore — The following is a report on the code…
///   │   ✅ agent:2  explore — I have sufficient information for a…
///   │   …
/// ```
///
/// Style choices are deliberately conservative: status icon stays
/// uncoloured (the icon itself carries semantic meaning), task id is
/// `BOLD` for scanability, agent name is `DIM` (it's contextual, not
/// the headline), preview stays in default text colour so it stands
/// out from the prefix chrome.
#[must_use]
pub fn try_render_wait_task_lines(payload: &str) -> Option<Vec<Line<'static>>> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let tasks = v.get("tasks")?.as_array()?;
    if tasks.is_empty() {
        return None;
    }

    let summary = v.get("summary").and_then(|s| s.as_object());
    let count = |k: &str| -> u64 {
        summary
            .and_then(|s| s.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    let total = summary
        .and_then(|s| s.get("total"))
        .and_then(|v| v.as_u64())
        .unwrap_or(tasks.len() as u64);

    // Build the summary suffix in a fixed order so the rendered text is
    // stable across runs. Zero-count statuses are dropped so a fully
    // successful gather doesn't render as "✅ 4 completed · ⏱ 0 timed
    // out · ⛔ 0 cancelled · …".
    let mut summary_parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("completed", "completed"),
        ("timed_out", "timed out"),
        ("cancelled", "cancelled"),
        ("not_found", "not found"),
        ("forbidden", "forbidden"),
        ("parse_error", "parse error"),
    ] {
        let n = count(key);
        if n > 0 {
            summary_parts.push(format!("{} {n} {label}", wait_status_icon(key)));
        }
    }
    let summary_suffix = if summary_parts.is_empty() {
        String::new()
    } else {
        format!(" \u{2014} {}", summary_parts.join(" \u{00B7} "))
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header: "  │ N task(s) gathered — <summary>"
    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", TOOL_PREFIX),
        Span::styled(format!("{total} task(s) gathered{summary_suffix}"), BOLD),
    ]));

    // One row per task. We cap output preview to TUI_PREVIEW_CHARS;
    // longer reports stay reachable via `/debug-bundle` (full text in
    // the bundled `conversation.md`) or by re-asking the model to
    // summarise.
    let dim_italic = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    for task in tasks {
        let task_id = task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let icon = wait_status_icon(status);
        let agent_name = task
            .get("agent_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let preview = task
            .get("output")
            .and_then(|v| v.as_str())
            .map(|s| first_meaningful_line(s, TUI_PREVIEW_CHARS))
            .unwrap_or_default();

        // Spans are pushed conditionally so we don't emit empty
        // separators when agent_name or preview is missing.
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("  \u{2502} ", TOOL_PREFIX),
            Span::raw(format!("  {icon} ")),
            Span::styled(task_id.to_string(), BOLD),
        ];
        if !agent_name.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(agent_name.to_string(), DIM));
        }
        if !preview.is_empty() {
            spans.push(Span::raw(" \u{2014} "));
            spans.push(Span::styled(preview, dim_italic));
        }
        lines.push(Line::from(spans));
    }

    Some(lines)
}

/// Render a `ListBackgroundTasks` JSON payload (#1209).
///
/// Shape contract (see `koda-core::tools::bg_task_tools`):
///
/// ```json
/// [
///   {
///     "task_id": "agent:1",
///     "task_type": "agent" | "process",
///     "description": "<agent_name>: <prompt>" | "<command>",
///     "status": "pending"|"running"|"completed"|"cancelled"|"errored"
///             | "running"|"exited"|"killed",
///     "age_secs": 12,
///     "exit_code": 0   // optional, processes only
///   },
///   ...
/// ]
/// ```
///
/// Empty arrays render as a single "no background tasks" line so the
/// model's tool call still leaves a visible trace in history rather
/// than vanishing. Any other shape mismatch returns `None` so the
/// caller falls back to the generic raw render — we never want to
/// drop content silently.
///
/// Output shape (matches the `WaitTask` renderer's per-task style):
///
/// ```text
///   │ 3 background task(s)
///   │   ▶ agent:1  explore: scan koda-core/src for ToolEffect (running, 12s)
///   │   ◐ agent:2  verify: confirm render (pending, 3s)
///   │   ⛔ process:5821  cargo test --workspace (killed, 45s)
/// ```
#[must_use]
pub fn try_render_list_bg_tasks_lines(payload: &str) -> Option<Vec<Line<'static>>> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let tasks = v.as_array()?;

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Header. Empty-array case still emits a header so a `ListBackgroundTasks`
    // call with zero results is visible in history rather than rendering
    // as nothing-at-all (which would look like a broken tool).
    let header = if tasks.is_empty() {
        "no background tasks".to_string()
    } else {
        format!("{} background task(s)", tasks.len())
    };
    lines.push(Line::from(vec![
        Span::styled("  \u{2502} ", TOOL_PREFIX),
        Span::styled(header, BOLD),
    ]));

    let dim_italic = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::ITALIC);

    for task in tasks {
        let task_id = task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
        let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let age_secs = task.get("age_secs").and_then(|v| v.as_u64()).unwrap_or(0);
        let description = task
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| first_meaningful_line(s, TUI_PREVIEW_CHARS))
            .unwrap_or_default();
        let exit_code = task.get("exit_code").and_then(|v| v.as_i64());

        let icon = wait_status_icon(status);

        // Status suffix shows the raw status word (with optional exit
        // code for processes) plus age. Format mirrors the live
        // `bg_activity_overlay` so eye sweeps between live + history
        // see the same tail.
        let status_tail = match exit_code {
            Some(code) => format!("({status} {code}, {age_secs}s)"),
            None => format!("({status}, {age_secs}s)"),
        };

        let mut spans: Vec<Span<'static>> = vec![
            Span::styled("  \u{2502} ", TOOL_PREFIX),
            Span::raw(format!("  {icon} ")),
            Span::styled(task_id.to_string(), BOLD),
        ];
        if !description.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(description, dim_italic));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(status_tail, DIM));
        lines.push(Line::from(spans));
    }

    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_covers_known_statuses() {
        assert_eq!(wait_status_icon("completed"), "\u{2705}");
        assert_eq!(wait_status_icon("timed_out"), "\u{23F1}");
        assert_eq!(wait_status_icon("cancelled"), "\u{26D4}");
        assert_eq!(wait_status_icon("not_found"), "\u{2753}");
        assert_eq!(wait_status_icon("forbidden"), "\u{1F512}");
        assert_eq!(wait_status_icon("parse_error"), "\u{26A0}");
    }

    #[test]
    fn icon_falls_back_visibly_for_unknown_status() {
        // The fallback must be a glyph (not empty/space) so a future
        // status added to the engine without updating this table is
        // visible in rendered output.
        let icon = wait_status_icon("brand_new_status");
        assert!(!icon.is_empty());
        assert_ne!(icon, " ");
    }

    #[test]
    fn first_meaningful_line_strips_markdown_and_truncates() {
        assert_eq!(
            first_meaningful_line("### Header\n\nReal content", 50),
            "Header"
        );
        assert_eq!(
            first_meaningful_line("> blockquote intro\nrest", 50),
            "blockquote intro"
        );
        // Truncation appends ellipsis (single Unicode char, not "...").
        let long = "a".repeat(200);
        let out = first_meaningful_line(&long, 10);
        assert!(out.ends_with('\u{2026}'), "expected ellipsis: {out}");
        assert_eq!(out.chars().count(), 11); // 10 + ellipsis
    }

    #[test]
    fn first_meaningful_line_collapses_interior_whitespace() {
        // Real LLM output often has tabs / multiple spaces inside a line.
        // The preview should read as one continuous sentence.
        assert_eq!(first_meaningful_line("foo   bar\tbaz", 50), "foo bar baz");
    }

    #[test]
    fn try_render_returns_none_on_garbage() {
        assert!(try_render_wait_task_lines("{not json").is_none());
        assert!(try_render_wait_task_lines("\"a string\"").is_none());
        assert!(try_render_wait_task_lines("null").is_none());
    }

    #[test]
    fn try_render_returns_none_on_missing_tasks() {
        // Missing or empty tasks → fall back to generic renderer rather
        // than emit an empty pretty block (which would silently swallow
        // a structurally-broken payload).
        assert!(try_render_wait_task_lines(r#"{"summary":{}}"#).is_none());
        assert!(try_render_wait_task_lines(r#"{"tasks":[]}"#).is_none());
    }

    #[test]
    fn try_render_emits_header_plus_one_line_per_task() {
        let payload = serde_json::json!({
            "summary": {"total": 2, "completed": 2},
            "tasks": [
                {"task_id": "agent:1", "status": "completed", "agent_name": "explore",
                 "output": "Found 3 issues in the parser."},
                {"task_id": "agent:2", "status": "completed", "agent_name": "explore",
                 "output": "All clear in the renderer."},
            ],
        });
        let lines = try_render_wait_task_lines(&payload.to_string()).expect("renders");
        assert_eq!(lines.len(), 3, "header + 2 task rows: {lines:?}");

        let line_text =
            |i: usize| -> String { lines[i].spans.iter().map(|s| s.content.as_ref()).collect() };
        assert!(line_text(0).contains("2 task(s) gathered"));
        assert!(line_text(0).contains("\u{2705} 2 completed"));
        assert!(line_text(1).contains("agent:1") && line_text(1).contains("Found 3 issues"));
        assert!(line_text(2).contains("agent:2") && line_text(2).contains("All clear"));
    }

    #[test]
    fn try_render_uses_distinct_icons_for_mixed_statuses() {
        let payload = serde_json::json!({
            "summary": {"total": 3, "completed": 1, "timed_out": 1, "cancelled": 1},
            "tasks": [
                {"task_id": "agent:1", "status": "completed", "agent_name": "explore",
                 "output": "OK."},
                {"task_id": "agent:2", "status": "timed_out", "agent_name": "explore"},
                {"task_id": "agent:3", "status": "cancelled", "agent_name": "explore"},
            ],
        });
        let lines = try_render_wait_task_lines(&payload.to_string()).expect("renders");
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all.contains("\u{2705}"), "completed icon present: {all}");
        assert!(all.contains("\u{23F1}"), "timed_out icon present: {all}");
        assert!(all.contains("\u{26D4}"), "cancelled icon present: {all}");
    }

    #[test]
    fn try_render_omits_optional_fields_cleanly() {
        // Missing agent_name and missing output should NOT produce
        // dangling separators ("  — " with nothing after).
        let payload = serde_json::json!({
            "summary": {"total": 1, "completed": 1},
            "tasks": [
                {"task_id": "agent:1", "status": "completed"},
            ],
        });
        let lines = try_render_wait_task_lines(&payload.to_string()).expect("renders");
        let row: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!row.contains("\u{2014} "), "no dangling em-dash: {row}");
        assert!(row.contains("agent:1"), "task id still present: {row}");
    }

    // --- ListBackgroundTasks renderer (#1209) ------------------------------

    fn collect(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn list_bg_tasks_renders_mixed_agent_and_process() {
        let payload = serde_json::json!([
            {
                "task_id": "agent:1",
                "task_type": "agent",
                "description": "explore: scan koda-core/src for ToolEffect",
                "status": "running",
                "age_secs": 12,
            },
            {
                "task_id": "agent:2",
                "task_type": "agent",
                "description": "verify: confirm render",
                "status": "pending",
                "age_secs": 3,
            },
            {
                "task_id": "process:5821",
                "task_type": "process",
                "description": "cargo test --workspace",
                "status": "killed",
                "age_secs": 45,
            },
        ]);
        let lines = try_render_list_bg_tasks_lines(&payload.to_string()).expect("renders");
        let all = collect(&lines);

        // Header summarises count
        assert!(
            all.contains("3 background task(s)"),
            "header missing: {all}"
        );

        // Each task id present
        for id in ["agent:1", "agent:2", "process:5821"] {
            assert!(all.contains(id), "task id {id} missing: {all}");
        }

        // Status icons (running, pending, killed)
        assert!(all.contains("\u{25B6}"), "running icon ▶ missing: {all}");
        assert!(all.contains("\u{25D0}"), "pending icon ◐ missing: {all}");
        assert!(all.contains("\u{26D4}"), "killed icon ⛔ missing: {all}");

        // Status tail with age
        assert!(all.contains("(running, 12s)"), "status tail missing: {all}");
        assert!(all.contains("(killed, 45s)"), "status tail missing: {all}");
    }

    #[test]
    fn list_bg_tasks_includes_exit_code_when_present() {
        let payload = serde_json::json!([
            {
                "task_id": "process:1",
                "task_type": "process",
                "description": "cargo build",
                "status": "exited",
                "age_secs": 7,
                "exit_code": 0,
            },
        ]);
        let lines = try_render_list_bg_tasks_lines(&payload.to_string()).expect("renders");
        let all = collect(&lines);
        assert!(all.contains("(exited 0, 7s)"), "exit code missing: {all}");
        assert!(all.contains("\u{2713}"), "exited icon ✓ missing: {all}");
    }

    #[test]
    fn list_bg_tasks_renders_empty_array_visibly() {
        // The model can call this tool when nothing's running.
        // The user must still see *something* in history rather than
        // the line silently disappearing.
        let lines = try_render_list_bg_tasks_lines("[]").expect("renders");
        let all = collect(&lines);
        assert!(
            all.contains("no background tasks"),
            "empty-state header missing: {all}"
        );
        assert_eq!(lines.len(), 1, "empty state is exactly one line");
    }

    #[test]
    fn list_bg_tasks_returns_none_for_non_array() {
        // Non-array payload — fall back to generic render path.
        assert!(try_render_list_bg_tasks_lines("{}").is_none());
        assert!(try_render_list_bg_tasks_lines("not json").is_none());
    }
}
