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
use crate::tui_output::{
    self, AMBER, BOLD, CYAN, DIM, MAGENTA, ORANGE, READ_CONTENT, RED, TOOL_PREFIX, WRITE_CONTENT,
    YELLOW,
};
use crate::widgets::status_bar::TurnStats;
use koda_core::engine::EngineEvent;
use koda_core::tools::{ToolEffect, classify_tool};
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
                // Flush partial line if the buffer is getting long (prevents
                // the UI from appearing frozen when models like Gemma 4 emit
                // long thinking stretches without newlines — issue #823).
                if self.think_buf.len() > 120 {
                    let partial = std::mem::take(&mut self.think_buf);
                    tui_output::emit_line(
                        buffer,
                        Line::from(vec![
                            Span::styled("  \u{2502} ", DIM),
                            Span::styled(partial, DIM),
                        ]),
                    );
                }
            }
            EngineEvent::ThinkingDone => {
                if !self.think_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.think_buf);
                    tui_output::emit_line(
                        buffer,
                        Line::from(vec![
                            Span::styled("  \u{2502} ", DIM),
                            Span::styled(remaining, DIM),
                        ]),
                    );
                }
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
                let (dot_style, detail) = tool_call_styles(&name, &args);
                tui_output::emit_line(
                    buffer,
                    Line::from(vec![
                        Span::raw(indent),
                        Span::styled("\u{25cf} ", dot_style),
                        Span::styled(name, BOLD),
                        Span::raw(" "),
                        Span::styled(detail, DIM),
                    ]),
                );
            }
            EngineEvent::ToolOutputLine {
                id,
                line,
                is_stderr,
            } => {
                self.streaming_tool_ids.insert(id.clone());
                // Determine content style from the tool type: read-only tools
                // (Read, Grep, List…) get a legible off-white; mutating tools
                // (Bash, Write, Edit…) stay dim — fixing #804 issue #3.
                let tool_name = self
                    .pending_tool_args
                    .get(&id)
                    .map(|(n, _)| n.as_str())
                    .unwrap_or("");
                let (prefix, content_style) = if is_stderr {
                    ("  \u{2502}e ", RED)
                } else if matches!(classify_tool(tool_name), ToolEffect::ReadOnly) {
                    ("  \u{2502} ", READ_CONTENT)
                } else {
                    ("  \u{2502} ", WRITE_CONTENT)
                };
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
                let file_ext = self
                    .pending_tool_args
                    .remove(&id)
                    .and_then(|(_, args)| extract_file_extension(&args));

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
            | EngineEvent::LoopCapReached { .. } => {
                // Handled by the event loop, not the renderer.
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
        }
    }

    /// Stop any running spinner (no-op in TUI mode).
    #[allow(dead_code)]
    pub fn stop_spinner(&mut self) {}
}

// ── Helper renderers ─────────────────────────────────────────

/// Get the dot color and detail string for a tool call banner.
fn tool_call_styles(name: &str, args: &serde_json::Value) -> (Style, String) {
    let dot_style = match name {
        "Bash" => ORANGE,
        "Read" | "Grep" | "Glob" | "List" => CYAN,
        "Write" | "Edit" => AMBER,
        "Delete" => RED,
        "WebFetch" => Style::new().fg(Color::Blue),
        _ => DIM,
    };

    let detail = match name {
        "Bash" => args
            .get("command")
            .or(args.get("cmd"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Read" | "Write" | "Edit" | "Delete" => args
            .get("file_path")
            .or(args.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Grep" | "Glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "WebFetch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };

    (dot_style, detail)
}

/// Render tool output with collapsing for long outputs.
/// Extract file extension from tool call args JSON.
/// Works for Read ("path") and Grep ("path") tool args.
fn extract_file_extension(args_json: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let path = args["path"].as_str()?;
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    Some(ext.to_string())
}

fn render_tool_output(
    buffer: &mut ScrollBuffer,
    name: &str,
    output: &str,
    verbose: bool,
    file_ext: Option<&str>,
) {
    use koda_core::truncate::{Truncated, truncate_for_display};

    if output.is_empty() {
        return;
    }

    // Collapse consecutive blank lines (3+ → 1) to reduce visual noise,
    // especially from WebFetch HTML-to-text conversion.
    let collapsed = collapse_blank_lines(output);
    let output = &collapsed;

    // Syntax highlighting for Read tool output
    let use_highlight = name == "Read" && file_ext.is_some();
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
            render_grep_line(buffer, line);
        } else if name == "List" {
            render_list_line(buffer, line);
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

/// Render a single list entry with directory/file coloring.
///
/// List output format: `d path/to/dir` (directory) or `  path/to/file` (file).
/// Directories are shown in bold, files colored by extension.
fn render_list_line(buffer: &mut ScrollBuffer, line: &str) {
    let is_dir = line.starts_with("d ");
    let path_str = if is_dir {
        &line[2..]
    } else {
        line.trim_start()
    };

    let style = if is_dir {
        Style::default().add_modifier(ratatui::style::Modifier::BOLD)
    } else {
        // Color files by extension category
        let ext = std::path::Path::new(path_str)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        match ext {
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "rb" | "java" | "c" | "cpp"
            | "h" | "cs" | "swift" | "kt" => Style::default().fg(Color::Green),
            "toml" | "yaml" | "yml" | "json" | "xml" | "ini" | "cfg" | "conf" => {
                Style::default().fg(Color::Yellow)
            }
            "md" | "txt" | "rst" | "adoc" => Style::default().fg(Color::White),
            "lock" | "sum" => Style::default().fg(Color::DarkGray),
            _ => Style::default().fg(Color::Reset),
        }
    };

    let prefix = if is_dir { "\u{1f4c1} " } else { "   " };
    tui_output::emit_line(
        buffer,
        Line::from(vec![
            Span::styled("  \u{2502} ", DIM),
            Span::raw(prefix),
            Span::styled(path_str.to_string(), style),
        ]),
    );
}

/// Render a single grep result line with the file path highlighted.
///
/// Grep output format: `file_path:line_number:content`
/// We highlight the file path in cyan and the line number in yellow.
fn render_grep_line(buffer: &mut ScrollBuffer, line: &str) {
    // Parse file:line:content format
    if let Some((file_and_line, content)) = line.split_once(':').and_then(|(file, rest)| {
        rest.split_once(':')
            .map(|(lineno, content)| (format!("{file}:{lineno}"), content))
    }) {
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::styled(file_and_line, Style::default().fg(Color::Cyan)),
                Span::styled(":", DIM),
                Span::raw(content.to_string()),
            ]),
        );
    } else {
        // Fallback: render as-is
        tui_output::emit_line(
            buffer,
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::raw(line.to_string()),
            ]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
