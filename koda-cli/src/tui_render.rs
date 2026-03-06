//! TUI renderer: converts EngineEvents to native ratatui `Line`s.
//!
//! All output is rendered as `ratatui::text::Line` / `Span` and written
//! above the viewport via `insert_before()`. No ANSI strings.

use crate::tui_output::{self, AMBER, BOLD, CYAN, DIM, GREEN, MAGENTA, ORANGE, RED, YELLOW};
use koda_core::engine::EngineEvent;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Style},
    text::{Line, Span},
};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// TUI-aware renderer that outputs above the viewport.
pub struct TuiRenderer {
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::display::ToolOutputHistory,
    /// When true, tool output is never collapsed.
    pub verbose: bool,
    /// Buffer for streaming text deltas (flushed line-by-line).
    text_buf: String,
    /// Buffer for streaming thinking deltas.
    think_buf: String,
    /// Set when an ApprovalRequest with a preview was shown.
    pub preview_shown: bool,
    /// Whether we've emitted the response banner for this turn.
    response_started: bool,
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            tool_history: crate::display::ToolOutputHistory::new(),
            verbose: false,
            text_buf: String::new(),
            think_buf: String::new(),
            preview_shown: false,
            response_started: false,
        }
    }

    /// Render an engine event above the viewport using native ratatui types.
    pub fn render_to_terminal(&mut self, event: EngineEvent, terminal: &mut Term) {
        match event {
            EngineEvent::TextDelta { text } => {
                self.text_buf.push_str(&text);
                // Flush complete lines
                while let Some(pos) = self.text_buf.find('\n') {
                    let line_text = self.text_buf[..pos].to_string();
                    self.text_buf = self.text_buf[pos + 1..].to_string();
                    tui_output::emit_line(terminal, Line::raw(&line_text));
                }
            }
            EngineEvent::TextDone => {
                // Flush remaining partial line
                if !self.text_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.text_buf);
                    tui_output::emit_line(terminal, Line::raw(&remaining));
                }
                self.response_started = false;
            }
            EngineEvent::ThinkingStart => {
                self.think_buf.clear();
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled("\u{1f4ad} Thinking...", DIM),
                    ]),
                );
            }
            EngineEvent::ThinkingDelta { text } => {
                self.think_buf.push_str(&text);
                while let Some(pos) = self.think_buf.find('\n') {
                    let line_text = self.think_buf[..pos].to_string();
                    self.think_buf = self.think_buf[pos + 1..].to_string();
                    tui_output::emit_line(
                        terminal,
                        Line::from(vec![
                            Span::styled("  \u{2502} ", DIM),
                            Span::styled(line_text, DIM),
                        ]),
                    );
                }
            }
            EngineEvent::ThinkingDone => {
                if !self.think_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.think_buf);
                    tui_output::emit_line(
                        terminal,
                        Line::from(vec![
                            Span::styled("  \u{2502} ", DIM),
                            Span::styled(remaining, DIM),
                        ]),
                    );
                }
            }
            EngineEvent::ResponseStart => {
                self.response_started = true;
                tui_output::emit_line(terminal, Line::styled("  \u{2500}\u{2500}\u{2500}", DIM));
            }
            EngineEvent::ToolCallStart {
                id: _,
                name,
                args,
                is_sub_agent,
            } => {
                let indent = if is_sub_agent { "  " } else { "" };
                let (dot_style, detail) = tool_call_styles(&name, &args);
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw(indent),
                        Span::styled("\u{25cf} ", dot_style),
                        Span::styled(&name, BOLD),
                        Span::raw(" "),
                        Span::styled(detail, DIM),
                    ]),
                );
            }
            EngineEvent::ToolCallResult {
                id: _,
                name,
                output,
            } => {
                self.tool_history.push(&name, &output);
                let is_diff_tool =
                    matches!(name.as_str(), "Write" | "Edit" | "Delete" | "MemoryWrite");
                if self.preview_shown && is_diff_tool {
                    // Compact: just show line count
                    let line_count = output.lines().count();
                    tui_output::emit_line(
                        terminal,
                        Line::from(vec![
                            Span::styled("  \u{2514} ", DIM),
                            Span::styled(format!("{name}: {line_count} line(s)"), DIM),
                        ]),
                    );
                } else {
                    render_tool_output(terminal, &name, &output, self.verbose);
                }
                self.preview_shown = false;
            }
            EngineEvent::SubAgentStart { agent_name } => {
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{1f916} Sub-agent: {agent_name}"), MAGENTA),
                    ]),
                );
            }
            EngineEvent::SubAgentEnd { .. } => {}
            EngineEvent::ApprovalRequest { .. }
            | EngineEvent::StatusUpdate { .. }
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
                    terminal,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{1f4cb} Would execute: {detail}"), YELLOW),
                    ]),
                );
                if let Some(preview) = preview {
                    let rendered = crate::diff_render::render(&preview);
                    for line in rendered.lines() {
                        tui_output::emit_line(
                            terminal,
                            Line::from(vec![Span::raw("  "), Span::raw(line.to_string())]),
                        );
                    }
                }
            }
            EngineEvent::Footer {
                prompt_tokens,
                completion_tokens,
                cache_read_tokens,
                thinking_tokens,
                total_chars,
                elapsed_ms,
                rate,
                context,
            } => {
                render_footer(
                    terminal,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                    thinking_tokens,
                    total_chars,
                    elapsed_ms,
                    rate,
                    &context,
                );
            }
            EngineEvent::SpinnerStart { .. } | EngineEvent::SpinnerStop => {
                // TUI mode: spinner state is in the status bar.
            }
            EngineEvent::TodoDisplay { content } => {
                render_todo(terminal, &content);
            }
            EngineEvent::Info { message } => {
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![Span::raw("  "), Span::styled(message, CYAN)]),
                );
            }
            EngineEvent::Warn { message } => {
                tui_output::emit_line(
                    terminal,
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("\u{26a0} {message}"), YELLOW),
                    ]),
                );
            }
            EngineEvent::Error { message } => {
                tui_output::emit_line(
                    terminal,
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
        "Think" | "ShareReasoning" => DIM,
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
fn render_tool_output(terminal: &mut Term, name: &str, output: &str, verbose: bool) {
    if output.is_empty() || name == "ShareReasoning" {
        return;
    }

    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let max_lines = if verbose { total } else { 4 };

    let show = total.min(max_lines);
    for line in &lines[..show] {
        tui_output::emit_line(
            terminal,
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::raw(line.to_string()),
            ]),
        );
    }
    if total > show {
        tui_output::emit_line(
            terminal,
            Line::from(vec![Span::styled(
                format!("  \u{2502} ... ({} more lines)", total - show),
                DIM,
            )]),
        );
    }
}

/// Render the inference footer with token stats.
#[allow(clippy::too_many_arguments)]
fn render_footer(
    terminal: &mut Term,
    prompt_tokens: i64,
    completion_tokens: i64,
    cache_read_tokens: i64,
    thinking_tokens: i64,
    total_chars: usize,
    elapsed_ms: u64,
    rate: f64,
    context: &str,
) {
    let display_tokens = if completion_tokens > 0 {
        completion_tokens
    } else {
        (total_chars / 4) as i64
    };
    let time_str =
        koda_core::inference::format_duration(std::time::Duration::from_millis(elapsed_ms));

    let mut parts = Vec::new();
    if prompt_tokens > 0 {
        parts.push(format!(
            "in: {}",
            koda_core::inference::format_token_count(prompt_tokens)
        ));
    }
    if display_tokens > 0 {
        parts.push(format!("out: {display_tokens}"));
    }
    parts.push(time_str);
    if display_tokens > 0 {
        parts.push(format!("{rate:.0} t/s"));
    }
    if cache_read_tokens > 0 {
        parts.push(format!(
            "cache: {} read",
            koda_core::inference::format_token_count(cache_read_tokens)
        ));
    }
    if thinking_tokens > 0 {
        parts.push(format!(
            "thinking: {}",
            koda_core::inference::format_token_count(thinking_tokens)
        ));
    }

    let mut footer = parts.join(" \u{00b7} ");
    if !context.is_empty() {
        footer.push_str(&format!(" \u{00b7} {context}"));
    }

    tui_output::emit_blank(terminal);
    tui_output::emit_line(terminal, Line::styled(footer, DIM));
}

/// Render a todo checklist.
fn render_todo(terminal: &mut Term, content: &str) {
    for line in content.lines() {
        let trimmed = line.trim();
        let style = if trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]") {
            GREEN
        } else if trimmed.starts_with("- [ ]") {
            YELLOW
        } else {
            DIM
        };
        tui_output::emit_line(
            terminal,
            Line::from(vec![Span::raw("  "), Span::styled(line.to_string(), style)]),
        );
    }
}
