//! TUI renderer: converts EngineEvents to native ratatui `Line`s.
//!
//! All output is rendered as `ratatui::text::Line` / `Span` and written
//! above the viewport via `insert_before()`. No ANSI strings.
//!
//! ## Pipeline
//!
//! ```text
//! EngineEvent (from koda_core)
//!     ↓
//! TuiRenderer::render_to_buffer()
//!     ↓  assembles styled Line<'static> values
//! ScrollBuffer::push()            ← render cache (VecDeque)
//!     ↓
//! tui_app  →  Paragraph::new(buffer.all_lines()).scroll()
//!     ↓
//! ratatui Terminal::draw()
//! ```
//!
//! ## Streaming text
//!
//! `TextDelta` events accumulate in `text_buf`. Complete lines (split at `\n`)
//! are flushed through the markdown renderer ([`crate::md_render`]) immediately
//! so the user sees progressive output. The partial tail is flushed on `TextDone`.
//!
//! ## Tool output
//!
//! `ToolOutputLine` events stream output in real time during long-running
//! tool calls (e.g. `cargo build`). When `ToolCallResult` arrives, if output
//! was already streamed the renderer just shows a compact exit-line summary
//! rather than duplicating all output.
//!
//! ## Verbose mode
//!
//! Set `TuiRenderer::verbose = true` to suppress the
//! [`koda_core::truncate`]-based collapsing. Every output line is shown.

use crate::ansi_parse::parse_ansi_spans;
use crate::scroll_buffer::ScrollBuffer;
use crate::theme;
use crate::tui_output::{self, CYAN, DIM, MAGENTA, RED, TOOL_PREFIX, YELLOW};
use crate::widgets::status_bar::TurnStats;
use koda_core::engine::EngineEvent;
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::HashMap;

/// TUI-aware renderer that outputs above the viewport.
pub struct TuiRenderer {
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::tool_history::ToolOutputHistory,
    /// When true, tool output is never collapsed.
    pub verbose: bool,
    /// Last turn stats for status bar display.
    pub last_turn_stats: Option<TurnStats>,
    /// Current model name displayed in the status bar.
    pub model: String,
    /// Buffer for streaming text deltas (flushed line-by-line).
    text_buf: String,
    /// Buffer for streaming thinking deltas.
    think_buf: String,
    /// Set when an ApprovalRequest with a preview was shown.
    pub preview_shown: bool,
    /// Whether we've emitted any text content for the current response.
    has_emitted_text: bool,
    /// Whether we've emitted the response banner for this turn.
    response_started: bool,
    /// Streaming markdown renderer.
    md: crate::md_render::MarkdownRenderer,
    /// Pending tool call args: maps tool_call_id → (tool_name, args_json).
    /// Used to extract file paths for syntax highlighting Read/Grep results.
    pending_tool_args: HashMap<String, (String, String)>,
    /// Tool IDs that emitted streaming output lines.
    /// Used to avoid re-rendering the full output in ToolCallResult.
    streaming_tool_ids: std::collections::HashSet<String>,
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            tool_history: crate::tool_history::ToolOutputHistory::new(),
            verbose: false,
            last_turn_stats: None,
            model: String::new(),
            text_buf: String::new(),
            think_buf: String::new(),
            preview_shown: false,
            has_emitted_text: false,
            response_started: false,
            md: crate::md_render::MarkdownRenderer::new(),
            pending_tool_args: HashMap::new(),
            streaming_tool_ids: std::collections::HashSet::new(),
        }
    }

    /// Render an engine event into the scroll buffer.
    pub fn render_to_buffer(&mut self, event: EngineEvent, buffer: &mut ScrollBuffer) {
        match event {
            EngineEvent::TextDelta { text } => {
                self.text_buf.push_str(&text);
                // Flush complete lines (skip leading blank lines)
                while let Some(pos) = self.text_buf.find('\n') {
                    let line_text = self.text_buf[..pos].to_string();
                    self.text_buf = self.text_buf[pos + 1..].to_string();
                    // Skip empty lines at the very start of a response
                    if line_text.is_empty() && !self.has_emitted_text {
                        continue;
                    }
                    self.has_emitted_text = true;
                    tui_output::emit_line(buffer, self.md.render_line(&line_text));
                }
            }
            EngineEvent::TextDone => {
                // Flush remaining partial line
                if !self.text_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.text_buf);
                    tui_output::emit_line(buffer, self.md.render_line(&remaining));
                }
                self.response_started = false;
                self.has_emitted_text = false;
                // Reset markdown state for the next response
                self.md = crate::md_render::MarkdownRenderer::new();
            }
            EngineEvent::ThinkingStart => {
                self.think_buf.clear();
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{1f4ad} Thinking...", DIM),
                    ]),
                );
            }
            EngineEvent::ThinkingDelta { text } => {
                self.think_buf.push_str(&text);
                // Emit complete lines immediately
                while let Some(pos) = self.think_buf.find('\n') {
                    let line_text = self.think_buf[..pos].to_string();
                    self.think_buf = self.think_buf[pos + 1..].to_string();
                    tui_output::emit_line(
                        buffer,
                        Line::from(vec![
                            Span::styled("  \u{2502} ", DIM),
                            Span::styled(line_text, DIM),
                        ]),
                    );
                }
            }
            EngineEvent::ThinkingDone if !self.think_buf.is_empty() => {
                let remaining = std::mem::take(&mut self.think_buf);
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::styled("  \u{2502} ", DIM),
                        Span::styled(remaining, DIM),
                    ]),
                );
            }
            EngineEvent::ResponseStart => {
                self.response_started = true;
                tui_output::emit_line(buffer, Line::styled("  \u{2500}\u{2500}\u{2500}", DIM));
            }
            EngineEvent::ToolCallStart {
                id,
                name,
                args,
                is_sub_agent,
            } => {
                // Track args for syntax highlighting in ToolCallResult
                self.pending_tool_args
                    .insert(id.clone(), (name.clone(), args.to_string()));
                let indent = if is_sub_agent { "  " } else { "" };
                let line = crate::tool_header::build_header_line(indent, &name, &args);
                tui_output::emit_line(buffer, line);
            }
            EngineEvent::ToolOutputLine {
                id,
                line,
                is_stderr,
            } => {
                self.streaming_tool_ids.insert(id.clone());
                // Pull the body color from the central theme module so
                // read-only output stays legible (#804) and stderr is
                // always red regardless of which tool produced it.
                let tool_name = self
                    .pending_tool_args
                    .get(&id)
                    .map(|(n, _)| n.as_str())
                    .unwrap_or("");
                let prefix = if is_stderr {
                    "  \u{2502}e "
                } else {
                    "  \u{2502} "
                };
                let content_style = theme::content_style_for(tool_name, is_stderr);
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::styled(prefix, TOOL_PREFIX),
                        Span::styled(line, content_style),
                    ]),
                );
            }
            EngineEvent::ToolCallResult { id, name, output } => {
                // If we streamed output lines, skip rendering the full result
                // (the user already saw it in real-time). Just show exit code.
                let streamed = self.streaming_tool_ids.remove(&id);
                let pending = self.pending_tool_args.remove(&id);
                let file_ext = pending
                    .as_ref()
                    .and_then(|(_, args)| extract_file_extension(args));
                // Grep needs the search pattern at render time so we can
                // bold the matched substring inside each result line.
                let grep_pattern = if name == "Grep" {
                    pending.as_ref().and_then(|(_, args)| {
                        serde_json::from_str::<serde_json::Value>(args)
                            .ok()
                            .and_then(|v| v["pattern"].as_str().map(str::to_string))
                    })
                } else {
                    None
                };

                self.tool_history.push(&name, &output);
                if streamed {
                    // Already streamed line-by-line — just show exit code summary.
                    let exit_line = output.lines().next().unwrap_or("");
                    tui_output::emit_line(
                        buffer,
                        Line::from(vec![
                            Span::styled("  \u{2514} ", DIM),
                            Span::styled(exit_line.to_string(), DIM),
                        ]),
                    );
                } else {
                    let is_diff_tool =
                        matches!(name.as_str(), "Write" | "Edit" | "Delete" | "MemoryWrite");
                    if self.preview_shown && is_diff_tool {
                        // Compact: just show line count
                        let line_count = output.lines().count();
                        tui_output::emit_line(
                            buffer,
                            Line::from(vec![
                                Span::styled("  \u{2514} ", DIM),
                                Span::styled(format!("{name}: {line_count} line(s)"), DIM),
                            ]),
                        );
                    } else {
                        render_tool_output(
                            buffer,
                            &name,
                            &output,
                            self.verbose,
                            file_ext.as_deref(),
                            grep_pattern.as_deref(),
                        );
                    }
                }
                self.preview_shown = false;
            }
            EngineEvent::SubAgentStart { agent_name } => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{1f916} Sub-agent: {agent_name}"), MAGENTA),
                    ]),
                );
            }
            EngineEvent::ApprovalRequest { .. }
            | EngineEvent::AskUserRequest { .. }
            | EngineEvent::StatusUpdate { .. }
            | EngineEvent::ContextUsage { .. }
            | EngineEvent::TurnStart { .. }
            | EngineEvent::TurnEnd { .. }
            | EngineEvent::LoopCapReached { .. }
            | EngineEvent::ChildTaskUpdate { .. }
            | EngineEvent::TodoUpdate { .. }
            | EngineEvent::ChildAgentActivity { .. } => {
                // Handled by the event loop, not the renderer.
                // (#1076) ChildTaskUpdate is routed to the bg-task panel,
                // which still reads from the registry snapshot for its
                // primary render. Future cleanup may have it consume
                // the event stream directly and drop the polling.
                // (#1077 Phase A) TodoUpdate is rendered by the TUI's
                // todo-list panel from the tool-result message stream;
                // the structured event is for ACP / headless clients
                // that want diff-driven animation. Same future-cleanup
                // path as ChildTaskUpdate.
                // (#1201 B2) ChildAgentActivity is dropped from TUI
                // scrollback render. The B1 inline-line render
                // (`[bg N] 🔧 Read foo.rs` per event) was redundant
                // with the post-completion `✅ Background agent X
                // completed\n<bullet trace>` Info line emitted by
                // `inference.rs` — a 50-tool bg agent surfaced 100
                // dim lines live + the same trace again at the end.
                // The Info line is the durable record; the status
                // pill and `/agents` panel cover "is it still
                // running". Headless/ACP keep the live lines because
                // those contexts are tail-friendly. A future Phase
                // B3 may add a Gemini-style live overlay above the
                // composer that renders activity from state without
                // touching scrollback.
            }
            EngineEvent::ActionBlocked {
                tool_name: _,
                detail,
                preview,
            } => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{1f50d} Would execute: {detail}"), YELLOW),
                    ]),
                );
                if let Some(preview) = preview {
                    let diff_lines = crate::diff_render::render_lines(&preview);
                    let gutter = crate::diff_render::GUTTER_WIDTH;
                    for line in diff_lines {
                        buffer.push_with_gutter(line, gutter);
                    }
                }
            }
            EngineEvent::Footer {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                total_chars,
                elapsed_ms,
                rate,
                ..
            } => {
                let tokens_out = if completion_tokens > 0 {
                    completion_tokens
                } else {
                    (total_chars / 4) as i64
                };
                self.last_turn_stats = Some(TurnStats {
                    tokens_in: prompt_tokens,
                    tokens_out,
                    cache_read: cache_read_tokens,
                    elapsed_ms,
                    rate,
                });
            }
            EngineEvent::SpinnerStart { .. } | EngineEvent::SpinnerStop => {
                // TUI mode: spinner state is in the status bar.
            }
            EngineEvent::Info { message } => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![Span::raw("  "), Span::styled(message, CYAN)]),
                );
            }
            EngineEvent::Warn { message } => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{26a0} {message}"), YELLOW),
                    ]),
                );
            }
            EngineEvent::Error { message } => {
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{2717} {message}"), RED),
                    ]),
                );
            }
            // Forward-compat: future EngineEvent variants are dropped
            // from scrollback until we wire a render. Same shape as
            // the SpinnerStart/Stop no-op arms above (#1224).
            _ => {}
        }
    }

    /// Stop any running spinner (no-op in TUI mode).
    #[allow(dead_code)]
    pub fn stop_spinner(&mut self) {}
}

// ── Helper renderers ──────────────────────────────────

/// Extract file extension from tool call args JSON.
///
/// Walks the keys koda's tools actually use (`file_path` first, then
/// legacy `path`). Returning [`None`] disables Read syntax highlighting
/// for that call — fixed in this PR after we discovered the previous
/// implementation only checked `path`, silently disabling highlighting
/// for every Read call (since koda's Read tool advertises `file_path`).
fn extract_file_extension(args_json: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = args
        .get("file_path")
        .or_else(|| args.get("path"))
        .and_then(|v| v.as_str())?;
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(ext.to_string())
}

fn render_tool_output(
    buffer: &mut ScrollBuffer,
    name: &str,
    output: &str,
    verbose: bool,
    file_ext: Option<&str>,
    grep_pattern: Option<&str>,
) {
    use koda_core::truncate::{Truncated, truncate_for_display};

    if output.is_empty() {
        return;
    }

    // WaitTask returns aggregated multi-task JSON (#1157) that's
    // user-hostile when dumped as a raw escape-soup blob. Pretty-print
    // it as a per-task summary instead, mirroring the markdown export
    // (transcript.rs). Falls back to the generic render path on any
    // parse failure so we never lose the raw content.
    if name == "WaitTask"
        && let Some(lines) = crate::wait_task_format::try_render_wait_task_lines(output)
    {
        for line in lines {
            tui_output::emit_line(buffer, line);
        }
        return;
    }

    // ListBackgroundTasks (#1209) — same JSON-soup problem as WaitTask;
    // the model calls this to peek at running bg work and we owe the
    // user a per-task render with the same icon vocabulary as the
    // live `child_activity_overlay` instead of a one-line JSON dump.
    if name == "ListBackgroundTasks"
        && let Some(lines) = crate::wait_task_format::try_render_list_bg_tasks_lines(output)
    {
        for line in lines {
            tui_output::emit_line(buffer, line);
        }
        return;
    }

    // Collapse consecutive blank lines (3+ → 1) to reduce visual noise,
    // especially from WebFetch HTML-to-text conversion.
    let collapsed = collapse_blank_lines(output);
    let output = &collapsed;

    // Syntax highlighting for Read tool output. Honors the global
    // KODA_SYNTAX_HIGHLIGHT=off kill switch (see [`theme`]).
    let use_highlight = name == "Read" && file_ext.is_some() && theme::syntax_highlight_enabled();
    let is_diff_tool = matches!(name, "Edit" | "Write" | "Delete");
    let mut highlighter = if use_highlight {
        Some(crate::highlight::CodeHighlighter::new(file_ext.unwrap()))
    } else {
        None
    };

    let render_line = |buffer: &mut ScrollBuffer,
                       line: &str,
                       hl: &mut Option<crate::highlight::CodeHighlighter>| {
        if name == "Grep" {
            crate::tool_output_lines::render_grep_line(buffer, line, grep_pattern);
        } else if name == "List" {
            crate::tool_output_lines::render_list_line(buffer, line);
        } else if name == "Glob" {
            crate::tool_output_lines::render_glob_line(buffer, line);
        } else if let Some(h) = hl.as_mut() {
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            spans.extend(h.highlight_spans(line));
            tui_output::emit_line(buffer, Line::from(spans));
        } else if is_diff_tool && line.starts_with('+') {
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::styled(line.to_string(), Style::default().fg(Color::Green)),
                ]),
            );
        } else if is_diff_tool && line.starts_with('-') {
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::styled(line.to_string(), Style::default().fg(Color::Red)),
                ]),
            );
        } else if is_diff_tool && line.starts_with('@') {
            tui_output::emit_line(
                buffer,
                Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::styled(line.to_string(), Style::default().fg(Color::Cyan)),
                ]),
            );
        } else {
            // Parse ANSI escape codes into native ratatui Spans.
            // Colored output from tools (cargo, git, pytest, etc.)
            // renders with proper styles instead of raw escape codes.
            let content_spans = parse_ansi_spans(line);
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            spans.extend(content_spans);
            tui_output::emit_line(buffer, Line::from(spans));
        }
    };

    if verbose {
        // Show everything in verbose mode
        for line in output.lines() {
            render_line(buffer, line, &mut highlighter);
        }
        return;
    }

    match truncate_for_display(output) {
        Truncated::Full(_) => {
            for line in output.lines() {
                render_line(buffer, line, &mut highlighter);
            }
        }
        Truncated::Split {
            head,
            tail,
            hidden,
            total,
        } => {
            for line in &head {
                render_line(buffer, line, &mut highlighter);
            }
            tui_output::emit_line(
                buffer,
                Line::from(vec![Span::styled(
                    koda_core::truncate::separator(hidden, total),
                    DIM,
                )]),
            );
            for line in &tail {
                render_line(buffer, line, &mut highlighter);
            }
        }
    }
}

/// Collapse runs of consecutive blank lines down to at most 1.
///
/// WebFetch HTML-to-text conversion often produces dozens of empty lines
/// from page footers, nav elements, etc. This keeps output scannable
/// without losing meaningful whitespace (single blank lines are preserved).
fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive_blanks = 0u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks <= 1 {
                result.push('\n');
            }
        } else {
            consecutive_blanks = 0;
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(line);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::event::EngineEvent;
    use serde_json::json;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Drive `TuiRenderer` with a complete Read tool call and return all
    /// buffer lines for inspection.
    fn render_read(file_path: &str, content: &str) -> Vec<ratatui::text::Line<'static>> {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.render_to_buffer(
            EngineEvent::ToolCallStart {
                id: "t1".into(),
                name: "Read".into(),
                args: json!({ "file_path": file_path }),
                is_sub_agent: false,
            },
            &mut buffer,
        );
        renderer.render_to_buffer(
            EngineEvent::ToolCallResult {
                id: "t1".into(),
                name: "Read".into(),
                output: content.to_string(),
            },
            &mut buffer,
        );
        buffer.all_lines().cloned().collect()
    }

    /// Drive `TuiRenderer` with any tool call (List/Glob/Grep/etc) and return
    /// the full buffer. The `args` value is whatever JSON the tool expects;
    /// the renderer just forwards it through `extract_file_extension` and the
    /// per-tool dispatch in `render_tool_output`.
    fn render_tool(
        tool_name: &str,
        args: serde_json::Value,
        output: &str,
    ) -> Vec<ratatui::text::Line<'static>> {
        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(256);
        renderer.render_to_buffer(
            EngineEvent::ToolCallStart {
                id: "t1".into(),
                name: tool_name.into(),
                args,
                is_sub_agent: false,
            },
            &mut buffer,
        );
        renderer.render_to_buffer(
            EngineEvent::ToolCallResult {
                id: "t1".into(),
                name: tool_name.into(),
                output: output.to_string(),
            },
            &mut buffer,
        );
        buffer.all_lines().cloned().collect()
    }

    /// Return all spans from lines whose *joined text* contains `needle`.
    fn spans_on_lines_containing(
        lines: &[ratatui::text::Line<'static>],
        needle: &str,
    ) -> Vec<ratatui::text::Span<'static>> {
        lines
            .iter()
            .filter(|line| {
                let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                joined.contains(needle)
            })
            .flat_map(|line| line.spans.iter().cloned())
            .collect()
    }

    // ── extract_file_extension ────────────────────────────────────────────────

    #[test]
    fn extract_file_extension_handles_file_path_key() {
        // The bug fix: koda’s tools advertise `file_path`, not `path`.
        // Pre-fix, this returned None and silently disabled Read
        // syntax highlighting on every Read call.
        let args = r#"{"file_path": "src/main.rs"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("rs"));
    }

    #[test]
    fn extract_file_extension_falls_back_to_path() {
        let args = r#"{"path": "src/lib.py"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("py"));
    }

    #[test]
    fn extract_file_extension_prefers_file_path_when_both_present() {
        let args = r#"{"file_path": "a.rs", "path": "b.py"}"#;
        assert_eq!(extract_file_extension(args).as_deref(), Some("rs"));
    }

    #[test]
    fn extract_file_extension_returns_none_for_extensionless() {
        let args = r#"{"file_path": "Makefile"}"#;
        assert_eq!(extract_file_extension(args), None);
    }

    // ── Read tool coloring regression tests ───────────────────────────────────
    // These drive the full TuiRenderer → ScrollBuffer pipeline to confirm that
    // Read output is actually syntax-highlighted, not rendered flat-white.
    // Each test seeds a specific extension so a future SyntaxSet regression
    // fails loudly here rather than silently in the user’s terminal.

    /// Headline regression: Cargo.toml was showing flat white because syntect’s
    /// default set had no TOML grammar. End-to-end proof it’s now colored.
    #[test]
    fn read_toml_produces_multiple_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read(
            "Cargo.toml",
            "[package]\nname = \"koda\"\nversion = \"0.2.16\"",
        );
        let spans = spans_on_lines_containing(&lines, "[package]");
        assert!(
            spans.len() > 2,
            "[package] line should have multiple colored spans (got {}); \
             TOML syntax highlighting broken — check two-face dep",
            spans.len()
        );
        // At least one content span must carry a non-default foreground.
        let colored = spans.iter().any(|s| {
            s.style
                .fg
                .is_some_and(|c| c != ratatui::style::Color::Reset)
        });
        assert!(
            colored,
            "no colored spans on [package] line — highlighter is a no-op"
        );
    }

    /// Rust was working before two-face; ensure the switch didn’t regress it.
    #[test]
    fn read_rust_still_produces_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read("src/main.rs", "fn main() {}\nprintln!(\"hello\");");
        let spans = spans_on_lines_containing(&lines, "fn main");
        assert!(
            spans.len() > 2,
            "`fn main` line should have multiple spans; Rust highlighting broken"
        );
    }

    /// TypeScript was silently broken (no grammar in syntect defaults).
    #[test]
    fn read_typescript_produces_colored_spans() {
        if !crate::theme::syntax_highlight_enabled() {
            return;
        }
        let lines = render_read(
            "index.ts",
            "interface Foo { bar: string }\nconst x: number = 42;",
        );
        let spans = spans_on_lines_containing(&lines, "interface");
        assert!(
            spans.len() > 2,
            "`interface` line should have multiple spans; TypeScript highlighting broken"
        );
    }

    /// Unknown extensions must not panic and must render as plain text
    /// (single content span, not zero spans).
    #[test]
    fn read_unknown_extension_falls_back_safely() {
        let lines = render_read("data.xyz123", "some content here");
        let spans = spans_on_lines_containing(&lines, "some content");
        assert!(
            !spans.is_empty(),
            "unknown-extension Read must still emit spans (got none)"
        );
    }

    /// Extensionless files (Makefile, Dockerfile) must render without panicking.
    #[test]
    fn read_extensionless_file_renders_safely() {
        let lines = render_read("Makefile", ".PHONY: all\nall:\n\techo done");
        // Just assert we get output lines containing the content.
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            all_text.contains(".PHONY"),
            "Makefile content missing from output"
        );
    }

    // ── Other read-only tools — E2E pipeline coverage ─────────────────────────
    // List / Glob / Grep have unit tests in tool_output_lines, but those
    // bypass the dispatch in render_tool_output. These tests catch the
    // class of bugs where a tool name typo or extension extraction quirk
    // silently flattens output — same shape as the Read tests above.

    /// List output: directory entries should render with non-default styling
    /// (bold), proving the dispatch → render_list_line path works end-to-end.
    #[test]
    fn list_directory_entries_render_styled() {
        let lines = render_tool(
            "List",
            json!({ "path": "." }),
            "d src\nd tests\n  Cargo.toml\n  README.md",
        );
        // Find the line containing "src" — directory entry.
        let src_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "src"));
        let src_line = src_line.expect("src directory entry not in output");
        let dir_span = src_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "src")
            .unwrap();
        assert!(
            dir_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "directory entry should be bold; render_list_line dispatch broken"
        );
    }

    /// List output: file entries get extension-bucket coloring (e.g. .toml → yellow).
    #[test]
    fn list_file_entries_colored_by_extension() {
        let lines = render_tool("List", json!({ "path": "." }), "  Cargo.toml\n  src.rs");
        let toml_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "Cargo.toml"))
            .expect("Cargo.toml entry not in output");
        let toml_span = toml_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Cargo.toml")
            .unwrap();
        assert_eq!(
            toml_span.style.fg,
            Some(ratatui::style::Color::Yellow),
            ".toml file should be yellow (config bucket); style_for_extension dispatch broken"
        );
    }

    /// Glob output: file paths get extension-bucket coloring; meta lines dim.
    #[test]
    fn glob_paths_colored_by_extension() {
        let lines = render_tool(
            "Glob",
            json!({ "pattern": "**/*.rs" }),
            "3 files matched:\nsrc/main.rs\nsrc/lib.rs\ntests/it.rs",
        );
        let rs_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref() == "src/main.rs"))
            .expect("src/main.rs not in Glob output");
        let rs_span = rs_line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "src/main.rs")
            .unwrap();
        assert_eq!(
            rs_span.style.fg,
            Some(ratatui::style::Color::Green),
            ".rs file should be green (source bucket); render_glob_line dispatch broken"
        );
    }

    /// Grep output: matched substrings inside content render with MATCH_HIT style.
    #[test]
    fn grep_pattern_hits_styled_with_match_hit() {
        let lines = render_tool(
            "Grep",
            json!({ "pattern": "TODO", "path": "." }),
            "src/main.rs:42:    // TODO: refactor this",
        );
        // The hit span should carry the MATCH_HIT style (amber + bold).
        let hit_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "TODO");
        let hit_span = hit_span.expect("TODO match span missing; Grep dispatch broken");
        assert!(
            hit_span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "Grep match should be bold (MATCH_HIT); pattern propagation broken"
        );
        assert_eq!(
            hit_span.style.fg,
            Some(ratatui::style::Color::Rgb(255, 191, 0)),
            "Grep match should be amber (MATCH_HIT)"
        );
    }

    /// Grep without a pattern (e.g. structured query) still renders path/lineno coloring.
    #[test]
    fn grep_path_and_lineno_styled_without_pattern() {
        let lines = render_tool(
            "Grep",
            json!({ "path": "." }),
            "src/main.rs:42:fn main() {}",
        );
        // File path span should be present and styled (cyan).
        let path_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.as_ref() == "src/main.rs");
        assert!(
            path_span.is_some(),
            "Grep file path span missing — render_grep_line dispatch broken"
        );
        assert!(
            path_span.unwrap().style.fg.is_some(),
            "Grep file path should have a foreground color"
        );
    }

    // ── collapse_blank_lines ────────────────────────────────────────────────────

    #[test]
    fn test_collapse_preserves_single_blank() {
        assert_eq!(collapse_blank_lines("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_collapse_many_blanks() {
        assert_eq!(collapse_blank_lines("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_collapse_no_blanks() {
        assert_eq!(collapse_blank_lines("a\nb\nc"), "a\nb\nc");
    }

    #[test]
    fn test_collapse_all_blank() {
        assert_eq!(collapse_blank_lines("\n\n\n\n"), "\n");
    }

    // ── #1201 B2: ChildAgentActivity is a TUI no-op ────────────────────────────
    //
    // The B1 PR rendered each child activity event as a dim inline
    // line in the scroll buffer. That turned out redundant with the
    // `✅ Background agent X completed\n<bullet trace>` Info line
    // emitted by `inference.rs` after `drain_completed()` — a 50-tool
    // bg agent surfaced 100 dim lines live + the same trace again at
    // the end. B2 drops the live render in TUI scrollback while
    // leaving headless/ACP untouched (those tail-friendly contexts
    // benefit from per-line streaming).
    //
    // This test pins that decision so a future refactor doesn't
    // accidentally re-introduce the noise.
    #[test]
    fn bg_child_activity_does_not_push_lines_to_scroll_buffer() {
        use koda_core::engine::event::{ChildAgentActivityKind, EngineEvent};

        let mut renderer = TuiRenderer::new();
        let mut buffer = ScrollBuffer::new(2500);

        for kind in [
            ChildAgentActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "src/main.rs".into(),
            },
            ChildAgentActivityKind::ToolEnd {
                tool_name: "Read".into(),
                success: true,
            },
            ChildAgentActivityKind::Info {
                message: "  \u{1f4cc} note".into(),
            },
        ] {
            renderer.render_to_buffer(
                EngineEvent::ChildAgentActivity {
                    task_id: 7,
                    spawner: None,
                    is_background: true,
                    kind,
                },
                &mut buffer,
            );
        }

        assert_eq!(
            buffer.len(),
            0,
            "ChildAgentActivity must not push to scrollback (#1201 B2); \
             post-completion `✅ ... completed` Info line is the durable record"
        );
    }

    // ── WaitTask / ListBackgroundTasks pretty-render wiring ──────────────────
    //
    // The `render_tool_output` dispatch has two opt-in branches that route
    // these tools through `wait_task_format` instead of the generic
    // line-by-line dump. The formatter has 9 unit tests; these two tests
    // cover the *wiring*: that the live TUI path actually invokes the
    // formatter and surfaces its output to the scroll buffer. Without
    // them, a one-character typo in the dispatch (e.g. `"WaitTasks"`
    // vs `"WaitTask"`) would silently fall through to raw-JSON dump and
    // only get caught visually. Closes the qa-expert finding from #1169.

    #[test]
    fn wait_task_branch_emits_pretty_lines_not_raw_json() {
        let payload = json!({
            "tasks": [{
                "task_id": "agent:7",
                "status": "completed",
                "agent_name": "explore",
                "output": "Found three call sites.",
            }],
            "summary": { "completed": 1, "total": 1 },
        })
        .to_string();

        let lines = render_tool("WaitTask", json!({}), &payload);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        // Pretty branch ran: header + completion icon + agent name + preview.
        assert!(
            joined.contains("1 task(s) gathered"),
            "missing pretty header — formatter wiring broken: {joined:?}"
        );
        assert!(joined.contains("\u{2705}"), "missing ✅ completion icon");
        assert!(joined.contains("explore"), "missing agent name");
        assert!(
            joined.contains("Found three call sites."),
            "missing output preview"
        );

        // Raw JSON keys must NOT leak through — that would mean the
        // dispatch fell through to the generic line-by-line path.
        assert!(
            !joined.contains("\"task_id\":") && !joined.contains("\"agent_name\":"),
            "raw JSON leaked — pretty branch did not fire: {joined:?}"
        );
    }

    #[test]
    fn list_bg_tasks_branch_emits_pretty_lines_not_raw_json() {
        let payload = json!([{
            "task_id": "agent:3",
            "status": "running",
            "age_secs": 12,
            "description": "verify the fix",
        }])
        .to_string();

        let lines = render_tool("ListBackgroundTasks", json!({}), &payload);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();

        assert!(
            joined.contains("1 background task(s)"),
            "missing pretty header — formatter wiring broken: {joined:?}"
        );
        assert!(joined.contains("verify the fix"), "missing description");

        assert!(
            !joined.contains("\"task_id\":") && !joined.contains("\"age_secs\":"),
            "raw JSON leaked — pretty branch did not fire: {joined:?}"
        );
    }
}
