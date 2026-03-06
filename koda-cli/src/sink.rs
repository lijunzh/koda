//! CLI sink — renders EngineEvents to the terminal.
//!
//! Two modes:
//! - **Direct** (headless): renders events inline, handles approvals with blocking I/O.
//! - **Channel** (REPL): forwards all events to the async event loop via `UiEvent`.

use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};

// ── UiEvent ──────────────────────────────────────────────────────

/// Events forwarded from `CliSink` to the main event loop.
pub(crate) enum UiEvent {
    Engine(EngineEvent),
}

// ── UiRenderer ───────────────────────────────────────────────────

/// Terminal renderer — owns the markdown streamer and spinner.
///
/// Handles all non-approval `EngineEvent` rendering. Owned by the
/// main event loop (channel mode) or by `CliSink` (direct mode).
pub(crate) struct UiRenderer {
    md: crate::markdown::MarkdownStreamer,
    spinner: Option<tokio::task::JoinHandle<()>>,
    /// Recent tool outputs for `/expand` replay.
    pub tool_history: crate::display::ToolOutputHistory,
    /// When true, tool output is never collapsed.
    pub verbose: bool,
    /// Buffer for streaming thinking deltas (rendered line-by-line).
    think_buf: String,
    /// Set when an ApprovalRequest with a preview was shown; cleared on next ToolCallResult.
    pub preview_shown: bool,
}

impl UiRenderer {
    pub fn new() -> Self {
        Self {
            md: crate::markdown::MarkdownStreamer::new(),
            spinner: None,
            tool_history: crate::display::ToolOutputHistory::new(),
            verbose: false,
            think_buf: String::new(),
            preview_shown: false,
        }
    }

    /// Render a non-approval engine event to the terminal.
    pub fn render(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::TextDelta { text } => {
                self.md.push(&text);
            }
            EngineEvent::TextDone => {
                self.md.flush();
            }
            EngineEvent::ThinkingStart => {
                self.think_buf.clear();
                crate::display::print_thinking_banner();
            }
            EngineEvent::ThinkingDelta { text } => {
                self.think_buf.push_str(&text);
                // Render complete lines as they arrive
                while let Some(newline_pos) = self.think_buf.find('\n') {
                    let line = self.think_buf[..newline_pos].to_string();
                    self.think_buf = self.think_buf[newline_pos + 1..].to_string();
                    crate::display::render_thinking_line(&line);
                }
            }
            EngineEvent::ThinkingDone => {
                // Flush remaining partial line
                if !self.think_buf.is_empty() {
                    let remaining = std::mem::take(&mut self.think_buf);
                    crate::display::render_thinking_line(&remaining);
                }
            }
            EngineEvent::ResponseStart => {
                crate::display::print_response_banner();
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
                crate::display::print_tool_call(&tc, is_sub_agent);
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
                    // Preview was already shown before confirmation — show compact summary
                    crate::display::print_tool_output_compact(&name, &output);
                } else if self.verbose {
                    if let Some(record) = self.tool_history.get(1) {
                        crate::display::print_tool_output_full(record);
                    }
                } else {
                    crate::display::print_tool_output(&name, &output);
                }
                self.preview_shown = false;
            }
            EngineEvent::SubAgentStart { agent_name } => {
                crate::display::print_sub_agent_start(&agent_name);
            }
            EngineEvent::SubAgentEnd { .. } => {}
            EngineEvent::ApprovalRequest { .. } => {
                // In channel mode: handled by the main event loop.
                // In direct mode: handled by CliSink::emit() before reaching here.
            }
            EngineEvent::ActionBlocked {
                tool_name: _,
                detail,
                preview,
            } => {
                println!("  \x1b[33m\u{1f4cb} Would execute: {detail}\x1b[0m");
                if let Some(ref preview) = preview {
                    let rendered = crate::diff_render::render(preview);
                    for line in rendered.lines() {
                        println!("  {line}");
                    }
                }
            }
            EngineEvent::StatusUpdate { .. } => {
                // Status bar updates are a TUI/server concern, not CLI.
            }
            EngineEvent::TurnStart { .. } => {
                // Turn lifecycle: handled by the event loop, not the renderer.
            }
            EngineEvent::TurnEnd { .. } => {
                // Turn lifecycle: handled by the event loop, not the renderer.
            }
            EngineEvent::LoopCapReached { .. } => {
                // Loop cap: handled by the event loop, not the renderer.
            }
            EngineEvent::TodoDisplay { content } => {
                print!("{}", crate::display::format_todo_display(&content));
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
                println!("\n\n\x1b[90m{footer}{ctx_part}\x1b[0m\n");
            }
            EngineEvent::SpinnerStart { message } => {
                self.start_spinner(message);
            }
            EngineEvent::SpinnerStop => {
                self.stop_spinner();
            }
            EngineEvent::Info { message } => {
                println!("  \x1b[36m{message}\x1b[0m");
            }
            EngineEvent::Warn { message } => {
                println!("  \x1b[33m\u{26a0} {message}\x1b[0m");
            }
            EngineEvent::Error { message } => {
                println!("  \x1b[31m\u{2717} {message}\x1b[0m");
            }
        }
    }

    fn start_spinner(&mut self, message: String) {
        self.stop_spinner();

        let handle = tokio::spawn(async move {
            let frames = ["⠋", "⠙", "⠸", "⠰", "⠠", "⠆", "⠎", "⠇"];
            let start = std::time::Instant::now();
            let mut i = 0usize;
            loop {
                let frame = frames[i % frames.len()];
                let elapsed = start.elapsed().as_secs();
                let display = if elapsed > 0 {
                    format!("{message} ({elapsed}s)")
                } else {
                    message.clone()
                };
                eprint!("\r\x1b[36m{frame}\x1b[0m {display}\x1b[K");
                let _ = std::io::Write::flush(&mut std::io::stderr());
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            }
        });

        self.spinner = Some(handle);
    }

    pub(crate) fn stop_spinner(&mut self) {
        if let Some(handle) = self.spinner.take() {
            handle.abort();
            eprint!("\r\x1b[K");
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
    }
}

// ── CliSink ──────────────────────────────────────────────────────

/// The CLI sink that renders EngineEvents to the terminal.
///
/// Operates in two modes:
/// - **Direct**: renders events inline and handles approvals (headless mode).
/// - **Channel**: forwards all events to a `UiEvent` channel (REPL async loop).
pub struct CliSink {
    mode: SinkMode,
}

enum SinkMode {
    /// Direct rendering — used by headless mode.
    Direct {
        renderer: Box<std::sync::Mutex<UiRenderer>>,
        cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
    },
    /// Channel forwarding — used by the async REPL event loop.
    Channel {
        ui_tx: tokio::sync::mpsc::Sender<UiEvent>,
    },
}

impl CliSink {
    /// Create a direct-rendering sink (headless mode).
    pub fn new(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) -> Self {
        Self {
            mode: SinkMode::Direct {
                renderer: Box::new(std::sync::Mutex::new(UiRenderer::new())),
                cmd_tx,
            },
        }
    }

    /// Create a channel-forwarding sink (REPL async event loop).
    pub fn channel(ui_tx: tokio::sync::mpsc::Sender<UiEvent>) -> Self {
        Self {
            mode: SinkMode::Channel { ui_tx },
        }
    }
}

impl EngineSink for CliSink {
    fn emit(&self, event: EngineEvent) {
        match &self.mode {
            SinkMode::Direct { renderer, cmd_tx } => {
                // ApprovalRequest requires blocking I/O — handle inline.
                if let EngineEvent::ApprovalRequest {
                    ref id,
                    ref tool_name,
                    ref detail,
                    ref preview,
                    ref whitelist_hint,
                } = event
                {
                    use crate::confirm::{self, Confirmation};
                    let decision = match confirm::confirm_tool_action(
                        tool_name,
                        detail,
                        preview.as_ref(),
                        whitelist_hint.as_deref(),
                    ) {
                        Confirmation::Approved => ApprovalDecision::Approve,
                        Confirmation::Rejected => ApprovalDecision::Reject,
                        Confirmation::RejectedWithFeedback(fb) => {
                            ApprovalDecision::RejectWithFeedback { feedback: fb }
                        }
                        Confirmation::AlwaysAllow => ApprovalDecision::AlwaysAllow,
                    };
                    let _ = cmd_tx.blocking_send(EngineCommand::ApprovalResponse {
                        id: id.clone(),
                        decision,
                    });
                } else if matches!(event, EngineEvent::LoopCapReached { .. }) {
                    // Headless/direct mode: auto-continue
                    let _ = cmd_tx.blocking_send(EngineCommand::LoopDecision {
                        action: koda_core::loop_guard::LoopContinuation::Continue200,
                    });
                } else {
                    renderer.lock().unwrap().render(event);
                }
            }
            SinkMode::Channel { ui_tx } => {
                let _ = ui_tx.try_send(UiEvent::Engine(event));
            }
        }
    }
}
