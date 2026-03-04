//! CLI sink — renders EngineEvents to the terminal.
//!
//! This reproduces the exact current terminal output by delegating
//! to `display::` and `markdown::`. It's the default sink used in
//! interactive and headless modes.

use koda_core::engine::{ApprovalDecision, EngineCommand, EngineEvent, EngineSink};

/// The CLI sink that renders EngineEvents to the terminal.
pub struct CliSink {
    md: std::sync::Mutex<crate::markdown::MarkdownStreamer>,
    spinner: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
}

impl CliSink {
    pub fn new(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) -> Self {
        Self {
            md: std::sync::Mutex::new(crate::markdown::MarkdownStreamer::new()),
            spinner: std::sync::Mutex::new(None),
            cmd_tx,
        }
    }

    fn start_spinner(&self, message: String) {
        // Stop any existing spinner first
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

        *self.spinner.lock().unwrap() = Some(handle);
    }

    fn stop_spinner(&self) {
        if let Some(handle) = self.spinner.lock().unwrap().take() {
            handle.abort();
            eprint!("\r\x1b[K");
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
    }
}

impl EngineSink for CliSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::TextDelta { text } => {
                self.md.lock().unwrap().push(&text);
            }
            EngineEvent::TextDone => {
                self.md.lock().unwrap().flush();
            }
            EngineEvent::ThinkingStart => {
                crate::display::print_thinking_banner();
            }
            EngineEvent::ThinkingDelta { text } => {
                crate::display::render_thinking_block(&text);
            }
            EngineEvent::ThinkingDone => {}
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
                crate::display::print_tool_output(&name, &output);
            }
            EngineEvent::SubAgentStart { agent_name } => {
                crate::display::print_sub_agent_start(&agent_name);
            }
            EngineEvent::SubAgentEnd { .. } => {}
            EngineEvent::ApprovalRequest {
                id,
                tool_name,
                detail,
                preview,
                whitelist_hint,
            } => {
                // Show terminal confirmation and send response through channel
                use crate::confirm::{self, Confirmation};
                let decision = match confirm::confirm_tool_action(
                    &tool_name,
                    &detail,
                    preview.as_deref(),
                    whitelist_hint.as_deref(),
                ) {
                    Confirmation::Approved => ApprovalDecision::Approve,
                    Confirmation::Rejected => ApprovalDecision::Reject,
                    Confirmation::RejectedWithFeedback(fb) => {
                        ApprovalDecision::RejectWithFeedback { feedback: fb }
                    }
                    Confirmation::AlwaysAllow => ApprovalDecision::AlwaysAllow,
                };
                let _ = self
                    .cmd_tx
                    .blocking_send(EngineCommand::ApprovalResponse { id, decision });
            }
            EngineEvent::ActionBlocked {
                tool_name: _,
                detail,
                preview,
            } => {
                println!("  \x1b[33m\u{1f4cb} Would execute: {detail}\x1b[0m");
                if let Some(preview_text) = preview {
                    for line in preview_text.lines() {
                        println!("  {line}");
                    }
                }
            }
            EngineEvent::StatusUpdate { .. } => {
                // Status bar updates are a TUI/server concern, not CLI.
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
}
