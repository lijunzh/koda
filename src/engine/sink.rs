//! Engine output sink trait and CLI adapter.
//!
//! The `EngineSink` trait abstracts how the engine delivers events to clients.
//! `CliSink` is the default implementation that renders events to the terminal
//! using the existing display/markdown infrastructure — preserving the exact
//! current user experience.

use super::event::EngineEvent;

/// Trait for consuming engine events.
///
/// Implementors decide how to render or transport events:
/// - `CliSink`: renders to terminal via `display::` and `markdown::`
/// - Future `AcpSink`: serializes over WebSocket
/// - Future `TestSink`: collects events for assertions
pub trait EngineSink: Send + Sync {
    /// Emit an engine event to the client.
    fn emit(&self, event: EngineEvent);
}

/// The CLI sink that renders EngineEvents to the terminal.
///
/// This reproduces the exact current terminal output by delegating
/// to `display::` and `println!()`. It's the default sink used in
/// interactive and headless modes.
pub struct CliSink;

impl CliSink {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CliSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineSink for CliSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::TextDelta { .. } | EngineEvent::TextDone => {
                // Streaming text is handled by MarkdownStreamer in inference.rs,
                // not through the sink (yet). These events are for future clients.
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
                let tc = crate::providers::ToolCall {
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
            EngineEvent::ApprovalRequest { .. } => {
                // Approval is handled separately via channels (see #41).
                // The CLI adapter will intercept this in the REPL loop.
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
                let time_str =
                    crate::inference::format_duration(std::time::Duration::from_millis(elapsed_ms));
                let mut parts = Vec::new();
                if prompt_tokens > 0 {
                    parts.push(format!(
                        "in: {}",
                        crate::inference::format_token_count(prompt_tokens)
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
                        crate::inference::format_token_count(cache_read_tokens)
                    ));
                }
                if thinking_tokens > 0 {
                    parts.push(format!(
                        "thinking: {}",
                        crate::inference::format_token_count(thinking_tokens)
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
            EngineEvent::SpinnerStart { .. } | EngineEvent::SpinnerStop => {
                // Spinner is managed by SimpleSpinner in inference.rs directly.
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

/// A sink that collects events into a Vec for testing.
#[derive(Debug, Default)]
pub struct TestSink {
    events: std::sync::Mutex<Vec<EngineEvent>>,
}

impl TestSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get all collected events.
    pub fn events(&self) -> Vec<EngineEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Get the count of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Check if no events were collected.
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }
}

impl EngineSink for TestSink {
    fn emit(&self, event: EngineEvent) {
        self.events.lock().unwrap().push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sink_collects_events() {
        let sink = TestSink::new();
        assert!(sink.is_empty());

        sink.emit(EngineEvent::ResponseStart);
        sink.emit(EngineEvent::TextDelta {
            text: "hello".into(),
        });
        sink.emit(EngineEvent::TextDone);

        assert_eq!(sink.len(), 3);
        let events = sink.events();
        assert!(matches!(events[0], EngineEvent::ResponseStart));
        assert!(matches!(&events[1], EngineEvent::TextDelta { text } if text == "hello"));
        assert!(matches!(events[2], EngineEvent::TextDone));
    }

    #[test]
    fn test_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TestSink>();
    }

    #[test]
    fn test_trait_object_works() {
        let sink: Box<dyn EngineSink> = Box::new(TestSink::new());
        sink.emit(EngineEvent::Info {
            message: "test".into(),
        });
    }
}
