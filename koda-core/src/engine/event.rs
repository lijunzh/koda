//! Protocol types for engine ↔ client communication.
//!
//! These types form the contract between the Koda engine and any client surface.
//! They are serde-serializable so they can be sent over in-process channels
//! (CLI mode) or over the wire (ACP server mode).
//!
//! ## Design (DESIGN.md)
//!
//! - **Engine as a Library, Not a Process (P2, P3)**: The engine communicates
//!   exclusively through these enums. Zero IO in the engine crate.
//! - **Async Approval Flow (P3)**: `ApprovalRequest` / `ApprovalResponse` is
//!   async request/response, not a blocking call. Works identically over
//!   in-process channels or network transport.
//!
//! ### Principles
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
    TextDelta {
        /// The text chunk.
        text: String,
    },

    /// The LLM finished streaming text. Flush any buffered output.
    TextDone,

    /// The LLM started a thinking/reasoning block.
    ThinkingStart,

    /// A chunk of thinking/reasoning content.
    ThinkingDelta {
        /// The thinking text chunk.
        text: String,
    },

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

    /// A line of streaming output from a tool (currently Bash only).
    ///
    /// Emitted as each line arrives from stdout/stderr, before `ToolCallResult`.
    /// Clients can render these in real-time for a "live terminal" feel.
    ToolOutputLine {
        /// Matches the `id` from `ToolCallStart`.
        id: String,
        /// The output line (no trailing newline).
        line: String,
        /// Whether this line came from stderr.
        is_stderr: bool,
    },

    // ── Sub-agent delegation ──────────────────────────────────────────
    /// A sub-agent is being invoked.
    SubAgentStart {
        /// Name of the sub-agent being invoked.
        agent_name: String,
    },

    /// A sub-agent finished.

    // ── Todo list lifecycle (#1077 Phase A) ───────────────────────
    /// The model called `TodoWrite` and the engine accepted the new
    /// list. Emitted exactly once per accepted call (skipped when the
    /// new list is byte-identical to the previous one — the
    /// dedup-nudge path returns the "unchanged" message to the model
    /// without surfacing a transition to clients).
    ///
    /// Carries the full new list AND a server-computed diff against
    /// the previously persisted list so every client renders the
    /// same animation primitives (added / changed / removed) without
    /// having to maintain its own previous-list snapshot.
    ///
    /// Establishes the principle from `DESIGN.md § Progress Tracking:
    /// Model-Owned, History-Persisted, Engine-Surfaced` — the engine
    /// surfaces transitions, the conversation history persists the
    /// list, the system prompt does not re-inject it.
    TodoUpdate {
        /// The full todo list as written by the model on this call.
        items: Vec<crate::tools::todo::TodoItem>,
        /// Server-computed diff against the previously persisted list
        /// (matched by `content` string). On the first write of a
        /// session, every item shows up in `added`.
        diff: crate::tools::todo::TodoDiff,
    },

    // ── Background sub-agent lifecycle ────────────────────────────────
    /// A background sub-agent's status changed.
    ///
    /// Emitted on every transition through [`crate::bg_agent::AgentStatus`]
    /// (`Pending` → `Running { iter }` → terminal). Drained from the
    /// registry's status queue inside the inference loop alongside
    /// [`crate::bg_agent::BgAgentRegistry::drain_completed`], so any sink
    /// (CLI / TUI / headless / ACP) sees the same event stream without
    /// having to poll the registry directly.
    ///
    /// Closes the engine/UI boundary leak documented in #1076 — prior to
    /// this variant the TUI was the only client that could see live bg
    /// status because it shared the process and grabbed
    /// `Arc<BgAgentRegistry>` straight out of `KodaSession`.
    BgTaskUpdate {
        /// Monotonic id assigned at `reserve()` time, stable for the
        /// lifetime of the task.
        task_id: u32,
        /// Sub-agent invocation id of the spawner, or `None` if the
        /// task was launched from the top-level loop. See
        /// [`crate::bg_agent::BgTaskSnapshot::spawner`].
        spawner: Option<u32>,
        /// New status. Includes `Running { iter }` heartbeats so
        /// clients can render iteration progress without polling.
        status: crate::bg_agent::AgentStatus,
    },

    /// Live activity from inside a running background sub-agent.
    ///
    /// **#1201 B**: pre-this-event the parent's TUI had no live signal
    /// from inside a bg agent — only `BgTaskUpdate` heartbeats
    /// (`Running { iter: N }`), which tell you "still going" but not
    /// "doing what". The narrative trace shipped via `BufferingSink`
    /// only surfaced at result-injection time.
    ///
    /// `BgChildActivity` is the live tap: each interesting event
    /// inside the bg agent (tool start/end, info line) fans out to
    /// the parent's sink as soon as it happens, so the parent's TUI
    /// can render a Gemini-style activity feed under the bg-task's
    /// spawn cell. The post-completion narrative trace via
    /// `BufferingSink` is still emitted (and is still authoritative
    /// for the persisted transcript) — this event is purely for
    /// real-time UX.
    ///
    /// Same routing as `BgTaskUpdate`: pushed onto the registry's
    /// status-event queue by [`crate::bg_agent::BgStatusEmitter::send_activity`]
    /// and drained by the inference loop, so every client surface
    /// (TUI / headless / ACP) sees the same stream.
    BgChildActivity {
        /// Matches the `task_id` from `BgTaskUpdate` for the same
        /// running bg task.
        task_id: u32,
        /// Sub-agent invocation id of the spawner, or `None` for
        /// top-level-spawned bg tasks. Mirrors `BgTaskUpdate.spawner`.
        spawner: Option<u32>,
        /// What just happened inside the bg agent.
        kind: BgChildActivityKind,
    },

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
        /// The classified effect that triggered confirmation.
        effect: crate::tools::ToolEffect,
    },

    /// The model needs a clarifying answer from the user before proceeding.
    ///
    /// The client must respond with `EngineCommand::AskUserResponse`
    /// matching the same `id`. The answer is returned to the model as the
    /// tool result, so inference can continue.
    AskUserRequest {
        /// Unique ID for this request.
        id: String,
        /// The question to ask.
        question: String,
        /// Optional answer choices (empty = freeform).
        options: Vec<String>,
    },

    /// An action was blocked by safe mode (shown but not executed).
    ActionBlocked {
        /// Tool name that was blocked.
        tool_name: String,
        /// Description of the blocked action.
        detail: String,
        /// Diff preview (if applicable).
        preview: Option<crate::preview::DiffPreview>,
    },

    // ── Session metadata ──────────────────────────────────────────────
    /// Context window usage updated after assembling messages.
    ///
    /// Emitted once per inference turn so the client can display
    /// context percentage and trigger auto-compaction without reading
    /// engine-internal global state.
    ContextUsage {
        /// Tokens used in the current context window.
        used: usize,
        /// Maximum context window size.
        max: usize,
    },

    /// Progress/status update for the persistent status bar.
    StatusUpdate {
        /// Current model identifier.
        model: String,
        /// Current provider name.
        provider: String,
        /// Context window usage (0.0–1.0).
        context_pct: f64,
        /// Current approval mode label.
        approval_mode: String,
        /// Number of in-flight tool calls.
        active_tools: usize,
    },

    /// Inference completion footer with timing and token stats.
    Footer {
        /// Input tokens used.
        prompt_tokens: i64,
        /// Output tokens generated.
        completion_tokens: i64,
        /// Tokens read from cache.
        cache_read_tokens: i64,
        /// Tokens used for reasoning.
        thinking_tokens: i64,
        /// Total response characters.
        total_chars: usize,
        /// Wall-clock time in milliseconds.
        elapsed_ms: u64,
        /// Characters per second.
        rate: f64,
        /// Human-readable context usage string.
        context: String,
    },

    /// Spinner/progress indicator (presentational hint).
    ///
    /// Clients may render this as a terminal spinner, a status bar update,
    /// or ignore it entirely. The ratatui TUI uses the status bar instead.
    SpinnerStart {
        /// Status message to display.
        message: String,
    },

    /// Stop the spinner (presentational hint).
    ///
    /// See `SpinnerStart` — clients may ignore this.
    SpinnerStop,

    // ── Turn lifecycle ─────────────────────────────────────────────────
    /// An inference turn is starting.
    ///
    /// Emitted at the beginning of `inference_loop()`. Clients can use this
    /// to lock input, start timers, or update status indicators.
    TurnStart {
        /// Unique identifier for this turn.
        turn_id: String,
    },

    /// An inference turn has ended.
    ///
    /// Emitted when `inference_loop()` completes. Clients can use this to
    /// unlock input, drain type-ahead queues, or update status.
    TurnEnd {
        /// Matches the `turn_id` from `TurnStart`.
        turn_id: String,
        /// Why the turn ended.
        reason: TurnEndReason,
    },

    /// The engine's iteration hard cap was reached.
    ///
    /// The client must respond with `EngineCommand::LoopDecision`.
    /// Until the client responds, the inference loop is paused.
    LoopCapReached {
        /// The iteration cap that was hit.
        cap: u32,
        /// Recent tool names for context.
        recent_tools: Vec<String>,
    },

    // ── Messages ──────────────────────────────────────────────────────
    /// Informational message (not from the LLM).
    Info {
        /// The informational message.
        message: String,
    },

    /// Warning message.
    Warn {
        /// The warning message.
        message: String,
    },

    /// Error message.
    Error {
        /// The error message.
        message: String,
    },
}

/// What kind of activity happened inside a running background sub-agent.
///
/// **#1201 B**: deliberately a small, fixed set rather than "forward
/// every `EngineEvent`". The parent's TUI is rendering a *summary*
/// of child activity, not replaying the child's full event stream;
/// most events (streaming text deltas, thinking deltas, status
/// updates) would be noise at this granularity.
///
/// Wire format is `snake_case` with an internal `kind` tag, matching
/// the convention for [`TurnEndReason`] and
/// [`crate::bg_agent::AgentStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BgChildActivityKind {
    /// The child started a tool call.
    ///
    /// `summary` is a pre-truncated one-line description suitable
    /// for direct render (e.g. `"Read src/auth.rs"`, `"Bash cargo
    /// test"`). Computed at emit time so every client renders the
    /// same string without having to know the per-tool argument
    /// schema.
    ToolStart {
        /// Tool name (matches `EngineEvent::ToolCallStart.name`).
        tool_name: String,
        /// Pre-truncated one-line summary suitable for direct render.
        summary: String,
    },
    /// The child's tool call completed.
    ///
    /// Output is intentionally NOT included — it can be arbitrarily
    /// large and the parent's TUI is rendering a feed, not a
    /// transcript. The model's narrative trace via `BufferingSink`
    /// remains the authoritative record.
    ToolEnd {
        /// Tool name (matches `EngineEvent::ToolCallStart.name`).
        tool_name: String,
        /// Whether the tool succeeded. Best-effort classification
        /// at the emit site by inspecting the result string for an
        /// error-marker prefix; not load-bearing for correctness.
        success: bool,
    },
    /// An informational line from inside the child.
    ///
    /// These pass through verbatim from `EngineEvent::Info` so the
    /// child agent's own status messages (cache hit, microcompact
    /// fired, etc.) surface in the parent's feed.
    Info {
        /// The info line, rendered as-is.
        message: String,
    },
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
    Error {
        /// The error message.
        message: String,
    },
}

// ── Client → Engine ──────────────────────────────────────────────────────

/// Commands sent from the client to the engine.
///
/// Currently consumed variants:
/// - `ApprovalResponse` — during tool confirmation flow
/// - `Interrupt` — during approval waits and inference streaming
/// - `LoopDecision` — when iteration hard cap is reached
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineCommand {
    /// User requested interruption of the current operation.
    ///
    /// Consumed during approval waits. Also triggers `CancellationToken`
    /// for streaming interruption.
    Interrupt,

    /// Response to an `EngineEvent::AskUserRequest`.
    AskUserResponse {
        /// Must match the `id` from the `AskUserRequest`.
        id: String,
        /// The user's answer (empty string = cancelled).
        answer: String,
    },

    /// Response to an `EngineEvent::ApprovalRequest`.
    ApprovalResponse {
        /// Must match the `id` from the `ApprovalRequest`.
        id: String,
        /// The user's decision.
        decision: ApprovalDecision,
    },

    /// Response to an `EngineEvent::LoopCapReached`.
    ///
    /// Tells the engine whether to continue or stop after hitting
    /// the iteration hard cap.
    LoopDecision {
        /// Whether to continue or stop.
        action: crate::loop_guard::LoopContinuation,
    },

    /// User typed a message during inference and wants it injected into the
    /// **current** turn before the next provider request.
    ///
    /// The engine drains all pending `QueueNext` commands at the top of each
    /// loop iteration, batches them with `\n\n`, and inserts one user message
    /// into session history before re-querying the provider.  This is the
    /// "mid-turn steer" lane — the TUI's `later_queue` handles the separate
    /// "after this turn" lane entirely on the client side.
    QueueNext {
        /// The text the user submitted.
        text: String,
    },
}

/// The user's decision on an approval request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approve and execute the action.
    Approve,
    /// Reject the action (interactive: a human said no).
    Reject,
    /// Reject with feedback (tells the LLM what to change).
    RejectWithFeedback {
        /// Feedback explaining why the action was rejected.
        feedback: String,
    },
    /// Reject *automatically*, with no human in the loop. Distinct from
    /// [`ApprovalDecision::Reject`] because the model needs to know **why** it was
    /// rejected to act intelligently — a human "no" is a signal to
    /// re-plan or ask, but an auto-reject (e.g. headless mode
    /// refusing destructive ops by policy) is a structural constraint
    /// the model should adapt around for the rest of the session.
    ///
    /// **#1022 B15**: pre-fix, headless mode emitted `Reject` for
    /// auto-blocked destructive tools, which the model saw as `"User
    /// rejected this action."` — indistinguishable from a real human
    /// reject. The model would then ask the (nonexistent) user how to
    /// proceed, then time out.
    RejectAuto {
        /// Why the action was auto-rejected (surfaced to the model).
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_ask_user_request_roundtrip() {
        let event = EngineEvent::AskUserRequest {
            id: "ask-1".into(),
            question: "Which database?".into(),
            options: vec!["SQLite".into(), "PostgreSQL".into()],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("ask_user_request"));
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, EngineEvent::AskUserRequest { ref question, .. } if question == "Which database?")
        );
    }

    #[test]
    fn test_ask_user_response_roundtrip() {
        let cmd = EngineCommand::AskUserResponse {
            id: "ask-1".into(),
            answer: "SQLite".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("ask_user_response"));
        let deserialized: EngineCommand = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, EngineCommand::AskUserResponse { ref answer, .. } if answer == "SQLite")
        );
    }

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
            effect: crate::tools::ToolEffect::Destructive,
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
    fn test_approval_decision_variants() {
        let decisions = vec![
            ApprovalDecision::Approve,
            ApprovalDecision::Reject,
            ApprovalDecision::RejectWithFeedback {
                feedback: "try again".into(),
            },
            // #1022 B15: new variant for headless / no-human-in-loop
            // auto-rejection. Distinct from `Reject` on the wire so
            // the model can adapt its plan instead of asking a
            // nonexistent user.
            ApprovalDecision::RejectAuto {
                reason: "destructive op blocked by headless policy".into(),
            },
        ];
        for d in decisions {
            let json = serde_json::to_string(&d).unwrap();
            let roundtripped: ApprovalDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, roundtripped);
        }
    }

    /// #1022 B15: wire-format guard. The `decision` tag for the new
    /// `RejectAuto` variant must be `"reject_auto"` (snake_case via
    /// `#[serde(rename_all = "snake_case")]`). Renaming this would
    /// break ACP clients silently — they'd see an unknown decision
    /// and fall through to `Reject`, re-introducing the bug.
    #[test]
    fn test_reject_auto_wire_tag_is_snake_case() {
        let d = ApprovalDecision::RejectAuto { reason: "r".into() };
        let json = serde_json::to_string(&d).unwrap();
        assert!(
            json.contains("\"decision\":\"reject_auto\""),
            "expected snake_case tag, got: {json}"
        );
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
    fn test_queue_next_roundtrip() {
        let cmd = EngineCommand::QueueNext {
            text: "also add tests".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"queue_next\""));
        let deserialized: EngineCommand = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, EngineCommand::QueueNext { ref text } if text == "also add tests")
        );
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

    /// #1201 B: BgChildActivity must roundtrip cleanly so ACP / headless
    /// clients see the same wire shape as the in-process TUI. Tests all
    /// three kinds and the `BgChildActivity` envelope.
    #[test]
    fn test_bg_child_activity_roundtrip() {
        let kinds = vec![
            BgChildActivityKind::ToolStart {
                tool_name: "Read".into(),
                summary: "Read src/auth.rs".into(),
            },
            BgChildActivityKind::ToolEnd {
                tool_name: "Bash".into(),
                success: true,
            },
            BgChildActivityKind::ToolEnd {
                tool_name: "Edit".into(),
                success: false,
            },
            BgChildActivityKind::Info {
                message: "  \u{26a1} cache hit".into(),
            },
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let roundtripped: BgChildActivityKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, roundtripped);
        }

        // Envelope event — tests the outer EngineEvent serialization
        // including the snake_case type tag ("bg_child_activity").
        let event = EngineEvent::BgChildActivity {
            task_id: 7,
            spawner: Some(3),
            kind: BgChildActivityKind::ToolStart {
                tool_name: "Grep".into(),
                summary: "Grep TODO src/".into(),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"type\":\"bg_child_activity\""),
            "envelope must use snake_case type tag for ACP / headless clients"
        );
        assert!(
            json.contains("\"kind\":\"tool_start\""),
            "inner kind must use snake_case tag"
        );
        let deserialized: EngineEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            EngineEvent::BgChildActivity {
                task_id: 7,
                spawner: Some(3),
                ..
            }
        ));

        // Top-level-spawned bg task — spawner is None.
        let top_level = EngineEvent::BgChildActivity {
            task_id: 1,
            spawner: None,
            kind: BgChildActivityKind::Info {
                message: "hello".into(),
            },
        };
        let json = serde_json::to_string(&top_level).unwrap();
        let _: EngineEvent = serde_json::from_str(&json).unwrap();
    }
}
