//! Protocol types for engine ↔ client communication.
//!
//! These types form the contract between the Koda engine and any client surface.
//! They are serde-serializable so they can be sent over in-process channels
//! (CLI mode) or over the wire (ACP server mode).
//!
//! # Design Principles
//!
//! - **Semantic, not presentational**: Events describe *what happened*, not
//!   *how to render it*. The client decides formatting.
//! - **Bidirectional**: The engine emits `EngineEvent`s and accepts `EngineCommand`s.
//!   Some commands (like approval) are request/response pairs.
//! - **Serde-first**: All types derive `Serialize`/`Deserialize` for future
//!   wire transport (ACP/WebSocket).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Engine → Client ──────────────────────────────────────────────────────

/// Events emitted by the engine to the client.
///
/// The client is responsible for rendering these events appropriately
/// for its medium (terminal, GUI, JSON stream, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    // ── Streaming LLM output ──────────────────────────────────────────
    /// A chunk of streaming text from the LLM response.
    TextDelta { text: String },

    /// The LLM finished streaming text. Flush any buffered output.
    TextDone,

    /// The LLM started a thinking/reasoning block.
    ThinkingStart,

    /// A chunk of thinking/reasoning content.
    ThinkingDelta { text: String },

    /// The thinking/reasoning block finished.
    ThinkingDone,

    /// The LLM response section is starting (shown after thinking ends).
    ResponseStart,

    // ── Tool execution ────────────────────────────────────────────────
    /// A tool call is about to be executed.
    ToolCallStart {
        /// Unique ID for this tool call (from the LLM).
        id: String,
        /// Tool name (e.g., "Bash", "Read", "Edit").
        name: String,
        /// Tool arguments as JSON.
        args: Value,
        /// Whether this is a sub-agent's tool call.
        is_sub_agent: bool,
    },

    /// A tool call completed with output.
    ToolCallResult {
        /// Matches the `id` from `ToolCallStart`.
        id: String,
        /// Tool name.
        name: String,
        /// The tool's output text.
        output: String,
    },

    // ── Sub-agent delegation ──────────────────────────────────────────
    /// A sub-agent is being invoked.
    SubAgentStart { agent_name: String },

    /// A sub-agent finished.

    // ── Approval flow ─────────────────────────────────────────────────
    /// The engine needs user approval before executing a tool.
    ///
    /// The client must respond with `EngineCommand::ApprovalResponse`
    /// matching the same `id`.
    ApprovalRequest {
        /// Unique ID for this approval request.
        id: String,
        /// Tool name requiring approval.
        tool_name: String,
        /// Human-readable description of the action.
        detail: String,
        /// Structured diff preview (rendered by the client).
        preview: Option<crate::preview::DiffPreview>,
        /// If set, the client can offer an "Always allow" option for this pattern.
        whitelist_hint: Option<String>,
    },

    /// An action was blocked by plan mode (shown but not executed).
    ActionBlocked {
        tool_name: String,
        detail: String,
        preview: Option<crate::preview::DiffPreview>,
    },

    // ── Session metadata ──────────────────────────────────────────────
    /// Progress/status update for the persistent status bar.
    StatusUpdate {
        model: String,
        provider: String,
        context_pct: f64,
        approval_mode: String,
        active_tools: usize,
    },

    /// Inference completion footer with timing and token stats.
    Footer {
        prompt_tokens: i64,
        completion_tokens: i64,
        cache_read_tokens: i64,
        thinking_tokens: i64,
        total_chars: usize,
        elapsed_ms: u64,
        rate: f64,
        context: String,
    },

    /// Spinner/progress indicator (presentational hint).
    ///
    /// Clients may render this as a terminal spinner, a status bar update,
    /// or ignore it entirely. The ratatui TUI uses the status bar instead.
    SpinnerStart { message: String },

    /// Stop the spinner (presentational hint).
    ///
    /// See `SpinnerStart` — clients may ignore this.
    SpinnerStop,

    // ── Turn lifecycle ─────────────────────────────────────────────────
    /// An inference turn is starting.
    ///
    /// Emitted at the beginning of `inference_loop()`. Clients can use this
    /// to lock input, start timers, or update status indicators.
    TurnStart { turn_id: String },

    /// An inference turn has ended.
    ///
    /// Emitted when `inference_loop()` completes. Clients can use this to
    /// unlock input, drain type-ahead queues, or update status.
    TurnEnd {
        turn_id: String,
        reason: TurnEndReason,
    },

    /// The engine's iteration hard cap was reached.
    ///
    /// The client must respond with `EngineCommand::LoopDecision`.
    /// Until the client responds, the inference loop is paused.
    LoopCapReached { cap: u32, recent_tools: Vec<String> },

    // ── Messages ──────────────────────────────────────────────────────
    /// Informational message (not from the LLM).
    Info { message: String },

    /// Warning message.
    Warn { message: String },

    /// Error message.
    Error { message: String },
}

/// Why an inference turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEndReason {
    /// The LLM produced a final text response (no more tool calls).
    Complete,
    /// The user or system cancelled the turn.
    Cancelled,
    /// The turn failed with an error.
    Error { message: String },
}

// ── Client → Engine ──────────────────────────────────────────────────────

/// Commands sent from the client to the engine.
///
/// Currently consumed variants:
/// - `ApprovalResponse` — during tool confirmation flow
/// - `Interrupt` — during approval waits and inference streaming
/// - `LoopDecision` — when iteration hard cap is reached
///
/// Future (server mode, v0.2.0):
/// - `UserPrompt`, `SlashCommand`, `Quit` — defined for wire protocol
///   completeness but currently handled client-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineCommand {
    /// User submitted a prompt.
    ///
    /// Currently handled client-side. Will be consumed by the engine
    /// in server mode (v0.2.0) for prompt queuing.
    UserPrompt {
        text: String,
        /// Base64-encoded images attached to the prompt.
        #[serde(default)]
        images: Vec<ImageAttachment>,
    },

    /// User requested interruption of the current operation.
    ///
    /// Consumed during approval waits. Also triggers `CancellationToken`
    /// for streaming interruption.
    Interrupt,

    /// Response to an `EngineEvent::ApprovalRequest`.
    ApprovalResponse {
        /// Must match the `id` from the `ApprovalRequest`.
        id: String,
        decision: ApprovalDecision,
    },

    /// Response to an `EngineEvent::LoopCapReached`.
    ///
    /// Tells the engine whether to continue or stop after hitting
    /// the iteration hard cap.
    LoopDecision {
        action: crate::loop_guard::LoopContinuation,
    },

    /// A slash command from the REPL.
    ///
    /// Currently handled client-side. Defined for wire protocol completeness.
    SlashCommand(SlashCommand),

    /// User requested to quit the session.
    ///
    /// Currently handled client-side. Defined for wire protocol completeness.
    Quit,
}

/// An image attached to a user prompt.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAttachment {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type (e.g., "image/png").
    pub mime_type: String,
}

/// The user's decision on an approval request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approve and execute the action.
    Approve,
    /// Reject the action.
    Reject,
    /// Reject with feedback (tells the LLM what to change).
    RejectWithFeedback { feedback: String },
    /// Approve AND whitelist this command pattern for future auto-approval.
    AlwaysAllow,
}

/// Slash commands that the client can send to the engine.
/// Not yet consumed outside the engine module — wired in v0.2.0 server mode.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SlashCommand {
    /// Compact the conversation by summarizing history.
    Compact,
    /// Switch to a specific model by name.
    SwitchModel { model: String },
    /// Switch to a specific provider.
    SwitchProvider { provider: String },
    /// List recent sessions.
    ListSessions,
    /// Delete a session by ID.
    DeleteSession { id: String },
    /// Set the approval/trust mode.
    SetTrust { mode: String },
    /// MCP server management command.
    McpCommand { args: String },
    /// Show token usage for this session.
    Cost,
    /// View or save memory.
    Memory { action: Option<String> },
    /// Show help / command list.
    Help,
    /// Inject a prompt as if the user typed it (used by /diff review, etc.).
    InjectPrompt { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_engine_event_text_delta_roundtrip() {
        let event = EngineEvent::TextDelta {
            text: "Hello world".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""));
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, EngineEvent::TextDelta { text } if text == "Hello world"));
    }

    #[test]
    fn test_engine_event_tool_call_roundtrip() {
        let event = EngineEvent::ToolCallStart {
            id: "call_123".into(),
            name: "Bash".into(),
            args: serde_json::json!({"command": "cargo test"}),
            is_sub_agent: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, EngineEvent::ToolCallStart { name, .. } if name == "Bash"));
    }

    #[test]
    fn test_engine_event_approval_request_roundtrip() {
        let event = EngineEvent::ApprovalRequest {
            id: "approval_1".into(),
            tool_name: "Bash".into(),
            detail: "rm -rf node_modules".into(),
            preview: None,
            whitelist_hint: Some("rm".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineEvent::ApprovalRequest { tool_name, .. } if tool_name == "Bash"
        ));
    }

    #[test]
    fn test_engine_event_footer_roundtrip() {
        let event = EngineEvent::Footer {
            prompt_tokens: 4400,
            completion_tokens: 251,
            cache_read_tokens: 0,
            thinking_tokens: 0,
            total_chars: 1000,
            elapsed_ms: 43200,
            rate: 5.8,
            context: "1.9k/32k (5%)".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineEvent::Footer {
                prompt_tokens: 4400,
                ..
            }
        ));
    }

    #[test]
    fn test_engine_event_simple_variants_roundtrip() {
        let variants = vec![
            EngineEvent::TextDone,
            EngineEvent::ThinkingStart,
            EngineEvent::ThinkingDone,
            EngineEvent::ResponseStart,
            EngineEvent::SpinnerStop,
            EngineEvent::Info {
                message: "hello".into(),
            },
            EngineEvent::Warn {
                message: "careful".into(),
            },
            EngineEvent::Error {
                message: "oops".into(),
            },
        ];
        for event in variants {
            let json = serde_json::to_string(&event).unwrap();
            let _: EngineEvent = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_engine_command_user_prompt_roundtrip() {
        let cmd = EngineCommand::UserPrompt {
            text: "fix the bug".into(),
            images: vec![],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"user_prompt\""));
        let deserialized: EngineCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineCommand::UserPrompt { text, .. } if text == "fix the bug"
        ));
    }

    #[test]
    fn test_engine_command_approval_roundtrip() {
        let cmd = EngineCommand::ApprovalResponse {
            id: "approval_1".into(),
            decision: ApprovalDecision::RejectWithFeedback {
                feedback: "use npm ci instead".into(),
            },
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: EngineCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineCommand::ApprovalResponse {
                decision: ApprovalDecision::RejectWithFeedback { .. },
                ..
            }
        ));
    }

    #[test]
    fn test_engine_command_slash_commands_roundtrip() {
        let commands = vec![
            EngineCommand::SlashCommand(SlashCommand::Compact),
            EngineCommand::SlashCommand(SlashCommand::SwitchModel {
                model: "gpt-4".into(),
            }),
            EngineCommand::SlashCommand(SlashCommand::Cost),
            EngineCommand::SlashCommand(SlashCommand::SetTrust {
                mode: "yolo".into(),
            }),
            EngineCommand::SlashCommand(SlashCommand::Help),
            EngineCommand::Interrupt,
            EngineCommand::Quit,
        ];
        for cmd in commands {
            let json = serde_json::to_string(&cmd).unwrap();
            let _: EngineCommand = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn test_approval_decision_variants() {
        let decisions = vec![
            ApprovalDecision::Approve,
            ApprovalDecision::Reject,
            ApprovalDecision::RejectWithFeedback {
                feedback: "try again".into(),
            },
            ApprovalDecision::AlwaysAllow,
        ];
        for d in decisions {
            let json = serde_json::to_string(&d).unwrap();
            let roundtripped: ApprovalDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, roundtripped);
        }
    }

    #[test]
    fn test_image_attachment_roundtrip() {
        let img = ImageAttachment {
            data: "base64data==".into(),
            mime_type: "image/png".into(),
        };
        let json = serde_json::to_string(&img).unwrap();
        let deserialized: ImageAttachment = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.mime_type, "image/png");
    }

    #[test]
    fn test_turn_lifecycle_roundtrip() {
        let start = EngineEvent::TurnStart {
            turn_id: "turn-1".into(),
        };
        let json = serde_json::to_string(&start).unwrap();
        assert!(json.contains("turn_start"));
        let _: EngineEvent = serde_json::from_str(&json).unwrap();

        let end_complete = EngineEvent::TurnEnd {
            turn_id: "turn-1".into(),
            reason: TurnEndReason::Complete,
        };
        let json = serde_json::to_string(&end_complete).unwrap();
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineEvent::TurnEnd {
                reason: TurnEndReason::Complete,
                ..
            }
        ));

        let end_error = EngineEvent::TurnEnd {
            turn_id: "turn-2".into(),
            reason: TurnEndReason::Error {
                message: "oops".into(),
            },
        };
        let json = serde_json::to_string(&end_error).unwrap();
        let _: EngineEvent = serde_json::from_str(&json).unwrap();

        let end_cancelled = EngineEvent::TurnEnd {
            turn_id: "turn-3".into(),
            reason: TurnEndReason::Cancelled,
        };
        let json = serde_json::to_string(&end_cancelled).unwrap();
        let _: EngineEvent = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_loop_cap_reached_roundtrip() {
        let event = EngineEvent::LoopCapReached {
            cap: 200,
            recent_tools: vec!["Bash".into(), "Edit".into()],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("loop_cap_reached"));
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineEvent::LoopCapReached { cap: 200, .. }
        ));
    }

    #[test]
    fn test_loop_decision_roundtrip() {
        use crate::loop_guard::LoopContinuation;

        let cmd = EngineCommand::LoopDecision {
            action: LoopContinuation::Continue50,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: EngineCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineCommand::LoopDecision {
                action: LoopContinuation::Continue50
            }
        ));

        let cmd_stop = EngineCommand::LoopDecision {
            action: LoopContinuation::Stop,
        };
        let json = serde_json::to_string(&cmd_stop).unwrap();
        let _: EngineCommand = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_turn_end_reason_variants() {
        let reasons = vec![
            TurnEndReason::Complete,
            TurnEndReason::Cancelled,
            TurnEndReason::Error {
                message: "failed".into(),
            },
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let roundtripped: TurnEndReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, roundtripped);
        }
    }
}
