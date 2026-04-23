//! Engine output sink trait.
//!
//! The `EngineSink` trait abstracts how the engine delivers events to clients.
//! Implementations decide how to render or transport events:
//! - `CliSink` (in koda-cli): renders to terminal
//! - Future `AcpSink`: serializes over WebSocket
//! - `TestSink`: collects events for assertions

use super::event::EngineEvent;

/// Trait for consuming engine events.
///
/// Implementors decide how to render or transport events:
/// - `CliSink`: renders to terminal via `display::` and `markdown::`
/// - Future `AcpSink`: serializes over WebSocket
/// - `TestSink`: collects events for assertions
pub trait EngineSink: Send + Sync {
    /// Emit an engine event to the client.
    fn emit(&self, event: EngineEvent);
}

/// A no-op sink that discards all events.
///
/// Used by background sub-agents that don't have a live channel to
/// the user. **#1022 B9**: superseded for bg-agent use by
/// [`BufferingSink`], which captures a narrative trace so the user
/// can see what the bg agent did at result-injection time. `NullSink`
/// is still useful for tests and for any future fully-detached
/// execution path.
pub struct NullSink;

impl EngineSink for NullSink {
    fn emit(&self, _event: EngineEvent) {}
}

/// A sink that buffers a *narrative trace* of bg-agent activity.
///
/// **#1022 B9**: pre-fix, bg agents ran with [`NullSink`] so every
/// event inside them — tool calls, info lines, approval requests,
/// errors — was silently dropped. The user only saw two lines: the
/// spawn message and the completion message. The model only saw the
/// final output. *What the bg agent actually did* was opaque.
///
/// `BufferingSink` records short, human-readable lines for events
/// that matter for traceability:
/// - `ToolCallStart` → `"  🔧 ToolName"`
/// - `Info` → forwarded as-is (sub-agent emits info for things like
///   nested spawn / cache hit)
/// - `ApprovalRequest` / `AskUserRequest` → short auto-reject note
///   (they auto-reject on closed channel — see B10)
/// - Streaming text (`TextDelta`/`TextDone`) is *not* recorded — the
///   final output already crosses the result oneshot, so capturing
///   text here would duplicate it.
///
/// Drained at result-injection time and emitted as a multi-line
/// `Info` event so the user sees `✅ bg agent X completed\n  🔧 Read\n
/// 🔧 Bash\n  …` instead of just `✅ bg agent X completed`.
///
/// Cap is intentionally generous (256 lines): a runaway bg agent
/// could otherwise grow this unboundedly. After the cap we record a
/// single `… (trace truncated at N lines)` marker and stop.
pub struct BufferingSink {
    lines: std::sync::Mutex<Vec<String>>,
    cap: usize,
}

impl BufferingSink {
    /// Create a buffering sink with the default 256-line cap.
    pub fn new() -> Self {
        Self::with_cap(256)
    }

    /// Create a buffering sink with a custom cap (mainly for tests).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            lines: std::sync::Mutex::new(Vec::new()),
            cap,
        }
    }

    /// Drain and return all buffered lines. The sink is empty after
    /// this returns.
    pub fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }

    /// Append a line, honoring the cap. Idempotent on the truncation
    /// marker so a single overflow only produces one marker.
    fn push_capped(&self, line: String) {
        let mut guard = self.lines.lock().unwrap();
        if guard.len() < self.cap {
            guard.push(line);
        } else if guard.last().map(|l| !l.starts_with('…')).unwrap_or(true) {
            // Cap reached — emit one truncation marker and stop.
            guard.push(format!("… (trace truncated at {} lines)", self.cap));
        }
    }
}

impl Default for BufferingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineSink for BufferingSink {
    fn emit(&self, event: EngineEvent) {
        match event {
            EngineEvent::ToolCallStart { name, .. } => {
                self.push_capped(format!("  \u{1f527} {name}"));
            }
            EngineEvent::Info { message } => {
                // Sub-agent already prefixes its own info lines with
                // two spaces and an emoji — forward as-is so the
                // visual hierarchy survives.
                self.push_capped(message);
            }
            EngineEvent::ApprovalRequest { tool_name, .. } => {
                // B10: bg agents have no user channel — these always
                // auto-reject. Record so the model's apparent
                // "failure to do X" is debuggable.
                self.push_capped(format!(
                    "  \u{2398} approval auto-rejected for {tool_name} (no user channel)"
                ));
            }
            EngineEvent::AskUserRequest { question, .. } => {
                self.push_capped(format!(
                    "  \u{2398} ask-user auto-skipped: {}",
                    question.chars().take(80).collect::<String>()
                ));
            }
            // Everything else (streaming text, thinking, status, etc.)
            // is intentionally dropped — either redundant with the
            // result oneshot or noisy without context.
            _ => {}
        }
    }
}

/// A sink that collects events into a Vec for testing.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct TestSink {
    events: std::sync::Mutex<Vec<EngineEvent>>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestSink {
    /// Create an empty test sink.
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

#[cfg(any(test, feature = "test-support"))]
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

    // ── BufferingSink (#1022 B9) ─────────────────────────────────

    #[test]
    fn buffering_sink_records_tool_calls_and_info() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::ToolCallStart {
            id: "t1".into(),
            name: "Read".into(),
            args: serde_json::json!({"path": "foo.txt"}),
            is_sub_agent: false,
        });
        sink.emit(EngineEvent::Info {
            message: "  \u{26a1} cache hit".into(),
        });
        sink.emit(EngineEvent::ToolCallStart {
            id: "t2".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            is_sub_agent: false,
        });

        let lines = sink.take_lines();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("Read"), "got: {}", lines[0]);
        assert!(lines[1].contains("cache hit"), "got: {}", lines[1]);
        assert!(lines[2].contains("Bash"), "got: {}", lines[2]);
    }

    #[test]
    fn buffering_sink_drops_streaming_text() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::TextDelta {
            text: "hello".into(),
        });
        sink.emit(EngineEvent::TextDelta {
            text: " world".into(),
        });
        sink.emit(EngineEvent::TextDone);
        sink.emit(EngineEvent::ThinkingDelta {
            text: "reasoning".into(),
        });
        // Streaming text crosses the result oneshot already — capturing
        // it here would duplicate the model's final output in the
        // user-facing trace.
        assert!(sink.take_lines().is_empty());
    }

    #[test]
    fn buffering_sink_records_auto_reject_for_approval() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::ApprovalRequest {
            id: "a1".into(),
            tool_name: "Delete".into(),
            detail: "foo.txt".into(),
            preview: None,
            effect: crate::tools::ToolEffect::Destructive,
        });
        let lines = sink.take_lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Delete"));
        assert!(
            lines[0].contains("auto-rejected"),
            "approval-without-channel must be marked as auto-rejected; got: {}",
            lines[0]
        );
    }

    #[test]
    fn buffering_sink_caps_runaway_traces() {
        let sink = BufferingSink::with_cap(3);
        for i in 0..10 {
            sink.emit(EngineEvent::Info {
                message: format!("line {i}"),
            });
        }
        let lines = sink.take_lines();
        // 3 real lines + 1 truncation marker. Marker is idempotent
        // even though we tried to push 7 more lines.
        assert_eq!(lines.len(), 4, "got: {lines:?}");
        assert!(lines.last().unwrap().starts_with('\u{2026}'));
        assert!(lines.last().unwrap().contains("truncated"));
    }

    #[test]
    fn buffering_sink_take_drains() {
        let sink = BufferingSink::new();
        sink.emit(EngineEvent::Info {
            message: "a".into(),
        });
        assert_eq!(sink.take_lines().len(), 1);
        // Second take returns empty — not a snapshot, a drain.
        assert!(sink.take_lines().is_empty());
    }

    #[test]
    fn buffering_sink_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BufferingSink>();
    }
}
