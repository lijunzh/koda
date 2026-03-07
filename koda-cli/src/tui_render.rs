//! TUI renderer: converts EngineEvents to native ratatui `Line`s.
//!
//! All output is rendered as `ratatui::text::Line` / `Span` and written
//! above the viewport via `insert_before()`. No ANSI strings.

use crate::tui_output::{self, AMBER, BOLD, CYAN, DIM, MAGENTA, ORANGE, RED, YELLOW};
use crate::widgets::status_bar::TurnStats;
use koda_core::engine::EngineEvent;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    style::{Color, Style},
    text::{Line, Span},
};
use std::collections::HashMap;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// TUI-aware renderer that outputs above the viewport.
pub struct TuiRenderer {
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::tool_history::ToolOutputHistory,
    /// When true, tool output is never collapsed.
    pub verbose: bool,
    /// Last turn stats for status bar display.
    pub last_turn_stats: Option<TurnStats>,
    /// Current model name (for cost estimation).
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
        }
    }

    /// Render an engine event above the viewport using native ratatui types.
    pub fn render_to_terminal(&mut self, event: EngineEvent, terminal: &mut Term) {
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
                    tui_output::emit_line(terminal, self.md.render_line(&line_text));
                }
            }
            EngineEvent::TextDone => {
                // Flush remaining partial line
                if !self.text_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.text_buf);
                    tui_output::emit_line(terminal, self.md.render_line(&remaining));
                }
                self.response_started = false;
                self.has_emitted_text = false;
                // Reset markdown state for the next response
                self.md = crate::md_render::MarkdownRenderer::new();
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
            EngineEvent::ToolCallResult { id, name, output } => {
                // Extract file path from pending args for syntax highlighting
                let file_ext = self
                    .pending_tool_args
                    .remove(&id)
                    .and_then(|(_, args)| extract_file_extension(&args));

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
                    render_tool_output(terminal, &name, &output, self.verbose, file_ext.as_deref());
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
                    let diff_lines = crate::diff_render::render_lines(&preview);
                    tui_output::emit_lines(terminal, &diff_lines);
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
                let cost_usd = crate::cost::estimate_turn_cost(
                    &self.model,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens,
                );
                self.last_turn_stats = Some(TurnStats {
                    tokens_in: prompt_tokens,
                    tokens_out,
                    cache_read: cache_read_tokens,
                    elapsed_ms,
                    rate,
                    cost_usd,
                });
            }
            EngineEvent::SpinnerStart { .. } | EngineEvent::SpinnerStop => {
                // TUI mode: spinner state is in the status bar.
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
    terminal: &mut Term,
    name: &str,
    output: &str,
    verbose: bool,
    file_ext: Option<&str>,
) {
    if output.is_empty() {
        return;
    }

    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();
    let max_lines = if verbose { total } else { 4 };
    let show = total.min(max_lines);

    // Syntax highlighting for Read tool output
    let use_highlight = name == "Read" && file_ext.is_some();
    let mut highlighter = if use_highlight {
        Some(crate::highlight::CodeHighlighter::new(file_ext.unwrap()))
    } else {
        None
    };

    for line in &lines[..show] {
        if name == "Grep" {
            render_grep_line(terminal, line);
        } else if name == "List" {
            render_list_line(terminal, line);
        } else if let Some(ref mut hl) = highlighter {
            let mut spans = vec![Span::styled("  \u{2502} ", DIM)];
            spans.extend(hl.highlight_spans(line));
            tui_output::emit_line(terminal, Line::from(spans));
        } else {
            tui_output::emit_line(
                terminal,
                Line::from(vec![
                    Span::styled("  \u{2502} ", DIM),
                    Span::raw(line.to_string()),
                ]),
            );
        }
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

/// Render a single list entry with directory/file coloring.
///
/// List output format: `d path/to/dir` (directory) or `  path/to/file` (file).
/// Directories are shown in blue+bold, files colored by extension.
fn render_list_line(terminal: &mut Term, line: &str) {
    let is_dir = line.starts_with("d ");
    let path_str = if is_dir {
        &line[2..]
    } else {
        line.trim_start()
    };

    let style = if is_dir {
        Style::default()
            .fg(Color::Rgb(0x00, 0x53, 0xe2)) // Walmart blue.100
            .add_modifier(ratatui::style::Modifier::BOLD)
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
        terminal,
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
fn render_grep_line(terminal: &mut Term, line: &str) {
    // Parse file:line:content format
    if let Some((file_and_line, content)) = line.split_once(':').and_then(|(file, rest)| {
        rest.split_once(':')
            .map(|(lineno, content)| (format!("{file}:{lineno}"), content))
    }) {
        tui_output::emit_line(
            terminal,
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
            terminal,
            Line::from(vec![
                Span::styled("  \u{2502} ", DIM),
                Span::raw(line.to_string()),
            ]),
        );
    }
}
