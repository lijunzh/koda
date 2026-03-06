//! TUI renderer: converts EngineEvents to terminal output via `insert_before()`.
//!
//! Wraps the existing `UiRenderer` (which renders to `println!`) and redirects
//! output through `TuiOutput::emit()` for the persistent viewport.
//!
//! This is a transitional layer — eventually the rendering code will produce
//! native ratatui types directly (tracked in #78 Step 2).

use crate::tui_output::TuiOutput;
use koda_core::engine::EngineEvent;
use ratatui::{Terminal, backend::CrosstermBackend};

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

/// TUI-aware renderer that outputs above the viewport.
///
/// Internally captures `println!` output by redirecting rendering
/// through string buffers, then emits via `insert_before()`.
pub struct TuiRenderer {
    md: crate::markdown::MarkdownStreamer,
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::display::ToolOutputHistory,
    /// When true, tool output is never collapsed.
    pub verbose: bool,
    /// Buffer for streaming thinking deltas.
    think_buf: String,
    /// Set when an ApprovalRequest with a preview was shown.
    pub preview_shown: bool,
    /// Buffer for collecting output lines.
    output_buf: Vec<String>,
}

impl TuiRenderer {
    pub fn new() -> Self {
        Self {
            md: crate::markdown::MarkdownStreamer::new(),
            tool_history: crate::display::ToolOutputHistory::new(),
            verbose: false,
            think_buf: String::new(),
            preview_shown: false,
            output_buf: Vec::new(),
        }
    }

    /// Render an engine event above the viewport.
    pub fn render_to_terminal(&mut self, event: EngineEvent, terminal: &mut Term) {
        // For now, render using the existing ANSI-producing code
        // by capturing the output and routing through insert_before.
        match event {
            EngineEvent::TextDelta { text } => {
                self.md.push(&text);
                // Flush any complete lines from the markdown streamer
                // The streamer prints directly — we'll capture this in a future refactor.
                // For now, markdown output goes through the streamer's internal println!
                // which will need raw mode newline handling.
            }
            EngineEvent::TextDone => {
                self.md.flush();
            }
            EngineEvent::ThinkingStart => {
                self.think_buf.clear();
                TuiOutput::emit(terminal, "  \x1b[90m\u{1f4ad} Thinking...\x1b[0m");
            }
            EngineEvent::ThinkingDelta { text } => {
                self.think_buf.push_str(&text);
                while let Some(pos) = self.think_buf.find('\n') {
                    let line = self.think_buf[..pos].to_string();
                    self.think_buf = self.think_buf[pos + 1..].to_string();
                    TuiOutput::emit(terminal, &format!("  \x1b[90m\u{2502} {line}\x1b[0m"));
                }
            }
            EngineEvent::ThinkingDone => {
                if !self.think_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.think_buf);
                    TuiOutput::emit(terminal, &format!("  \x1b[90m\u{2502} {remaining}\x1b[0m"));
                }
            }
            EngineEvent::ResponseStart => {
                TuiOutput::emit(terminal, "");
            }
            EngineEvent::ToolCallStart {
                id: _,
                name,
                args,
                is_sub_agent,
            } => {
                let tc = koda_core::providers::ToolCall {
                    id: String::new(),
                    function_name: name,
                    arguments: serde_json::to_string(&args).unwrap_or_default(),
                    thought_signature: None,
                };
                // Use existing display code but capture output
                let banner = crate::display::format_tool_call_banner(&tc, is_sub_agent);
                TuiOutput::emit(terminal, &banner);
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
                    let summary = crate::display::format_tool_output_compact_str(&name, &output);
                    TuiOutput::emit(terminal, &summary);
                } else if self.verbose {
                    if let Some(record) = self.tool_history.get(1) {
                        let full = crate::display::format_tool_output_full_str(record);
                        TuiOutput::emit(terminal, &full);
                    }
                } else {
                    let rendered = crate::display::format_tool_output_str(&name, &output);
                    TuiOutput::emit(terminal, &rendered);
                }
                self.preview_shown = false;
            }
            EngineEvent::SubAgentStart { agent_name } => {
                TuiOutput::emit(
                    terminal,
                    &format!("  \x1b[35m\u{1f916} Sub-agent: {agent_name}\x1b[0m"),
                );
            }
            EngineEvent::SubAgentEnd { .. } => {}
            EngineEvent::ApprovalRequest { .. } => {
                // Handled by the event loop, not the renderer.
            }
            EngineEvent::ActionBlocked {
                tool_name: _,
                detail,
                preview,
            } => {
                TuiOutput::emit(
                    terminal,
                    &format!("  \x1b[33m\u{1f4cb} Would execute: {detail}\x1b[0m"),
                );
                if let Some(preview) = preview {
                    let rendered = crate::diff_render::render(&preview);
                    for line in rendered.lines() {
                        TuiOutput::emit(terminal, &format!("  {line}"));
                    }
                }
            }
            EngineEvent::StatusUpdate { .. }
            | EngineEvent::TurnStart { .. }
            | EngineEvent::TurnEnd { .. }
            | EngineEvent::LoopCapReached { .. } => {
                // Handled by the event loop.
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
                let display_tokens = if completion_tokens > 0 {
                    completion_tokens
                } else {
                    (total_chars / 4) as i64
                };
                let time_str = koda_core::inference::format_duration(
                    std::time::Duration::from_millis(elapsed_ms),
                );
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
                let footer = parts.join(" \u{00b7} ");
                let ctx_part = if context.is_empty() {
                    String::new()
                } else {
                    format!(" \u{00b7} {context}")
                };
                TuiOutput::emit(terminal, "");
                TuiOutput::emit(
                    terminal,
                    &format!("\x1b[90m{footer}{ctx_part}\x1b[0m"),
                );
                TuiOutput::emit(terminal, "");
            }
            EngineEvent::SpinnerStart { message: _ } => {
                // In TUI mode, spinner is shown in status bar, not as terminal output.
                // TODO: update status bar state
            }
            EngineEvent::SpinnerStop => {}
            EngineEvent::TodoDisplay { content } => {
                let rendered = crate::display::format_todo_display(&content);
                TuiOutput::emit(terminal, &rendered);
            }
            EngineEvent::Info { message } => {
                TuiOutput::emit(terminal, &format!("  \x1b[36m{message}\x1b[0m"));
            }
            EngineEvent::Warn { message } => {
                TuiOutput::emit(
                    terminal,
                    &format!("  \x1b[33m\u{26a0} {message}\x1b[0m"),
                );
            }
            EngineEvent::Error { message } => {
                TuiOutput::emit(
                    terminal,
                    &format!("  \x1b[31m\u{2717} {message}\x1b[0m"),
                );
            }
        }
    }

    /// Stop any running spinner (no-op in TUI mode — spinner is status bar).
    pub fn stop_spinner(&mut self) {
        // In TUI mode, spinner state is in the status bar, not a terminal animation.
    }
}
