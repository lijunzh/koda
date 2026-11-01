//! The public client protocol: what `koda -p --output-format stream-json`
//! writes, one [`WireLine`] per line.
//!
//! [`EngineEvent`] stays the engine's internal enum and can change freely.
//! This module is the contract: [`project`] decides what each engine event
//! becomes on the wire, and it matches exhaustively so a new engine event
//! can't reach clients, or miss them, without a decision here.
//!
//! The in-process TUI doesn't use this module.
//!
//! ## Compatibility
//!
//! `docs/src/wire-schema.json` is generated from [`WireLine`], and a test
//! fails when it goes stale. Run this to refresh it, and raise
//! [`PROTOCOL_VERSION`] if the change breaks a reader:
//!
//! ```text
//! KODA_UPDATE_WIRE_SCHEMA=1 cargo test -p koda-core --lib engine::wire
//! ```
//!
//! Readers skip a `type` they don't know, unless it is an interactive request,
//! which they must refuse: skipping one leaves the sender waiting. The rule is
//! in `docs/src/headless.md`. `#[non_exhaustive]` plays no part in this. It
//! governs Rust matches, not JSON decoding, and serde rejects an unknown tag
//! unless the reader handles it.

use super::event::{
    CompactionStatus, CompactionTrigger, EngineEvent, LoopGuardAction, RetryReason, ToolOutcome,
    TurnEndReason, UsageTotals,
};
use crate::preview::DiffPreview;
use crate::tools::ToolEffect;
use crate::tools::todo::{TodoDiff, TodoItem};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

/// Protocol version announced by `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Capabilities of `koda -p`: one turn per process, and the process answers
/// its own requests, so a reader must not.
pub const ONE_SHOT_CAPABILITIES: &[&str] = &["one_shot", "self_answer"];

/// One line of the protocol.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WireLine {
    /// The session. On every line except `hello`, which comes before one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The turn. On every line from `turn_start` to `turn_end`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// On a sub-agent's lines, the id of the tool call that started it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// What happened.
    #[serde(flatten)]
    pub event: WireEvent,
}

impl WireLine {
    /// The process's first line.
    pub fn hello(capabilities: &[&str]) -> Self {
        Self {
            session_id: None,
            turn_id: None,
            parent_id: None,
            event: WireEvent::Hello {
                protocol_version: PROTOCOL_VERSION,
                koda_version: env!("CARGO_PKG_VERSION").to_string(),
                capabilities: capabilities.iter().map(ToString::to_string).collect(),
            },
        }
    }

    /// A line that belongs to a session but to no turn.
    pub fn for_session(session_id: &str, event: WireEvent) -> Self {
        Self {
            session_id: Some(session_id.to_string()),
            turn_id: None,
            parent_id: None,
            event,
        }
    }
}

/// The message on a line, tagged by `type`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    /// The process is up. Always the first line.
    Hello {
        /// Protocol version.
        protocol_version: u32,
        /// Koda's version.
        koda_version: String,
        /// What this process can do. Ignore names you don't know.
        capabilities: Vec<String>,
    },
    /// A session is ready. Comes after `hello` and before any turn.
    SessionReady {
        /// Provider name.
        provider: String,
        /// Model identifier.
        model: String,
        /// Context window in tokens.
        context_window: usize,
        /// Trust mode: `plan`, `safe` or `auto`.
        trust: String,
        /// Names of the tools the model is offered.
        tools: Vec<String>,
    },

    /// A turn began.
    TurnStart,
    /// A chunk of the reply.
    TextDelta {
        /// The chunk.
        text: String,
    },
    /// The reply so far is finished.
    TextDone,
    /// A reasoning block began.
    ThinkingStart,
    /// A chunk of reasoning.
    ThinkingDelta {
        /// The chunk.
        text: String,
    },
    /// The reasoning block finished.
    ThinkingDone,

    /// A tool call is starting.
    ToolCallStart {
        /// The call's id.
        id: String,
        /// Tool name.
        name: String,
        /// Tool arguments.
        args: Value,
    },
    /// A line of a running tool's output.
    ToolOutputLine {
        /// The call's id.
        id: String,
        /// The line, with no trailing newline.
        line: String,
        /// Whether it came from stderr.
        is_stderr: bool,
    },
    /// A top-level tool call finished. Calls that were rejected, blocked or
    /// cancelled, and calls made by sub-agents, don't get one in protocol 1.
    ToolCallResult {
        /// The call's id.
        id: String,
        /// Tool name.
        name: String,
        /// What the model sees.
        output: String,
        /// Whether it succeeded.
        outcome: ToolOutcome,
    },
    /// A sub-agent is starting. Its lines carry `parent_id`.
    SubAgentStart {
        /// The agent's name.
        agent_name: String,
    },
    /// The todo list changed.
    TodoUpdate {
        /// The full list.
        items: Vec<TodoItem>,
        /// What changed.
        diff: TodoDiff,
    },

    /// A tool call needs approval. Interactive: see the unknown-types rule.
    ApprovalRequest {
        /// The request's id.
        id: String,
        /// Tool name.
        tool_name: String,
        /// What the call would do.
        detail: String,
        /// Diff preview, if there is one.
        preview: Option<DiffPreview>,
        /// Why it needs approval.
        effect: ToolEffect,
    },
    /// The model asked a question. Interactive: see the unknown-types rule.
    AskUserRequest {
        /// The request's id.
        id: String,
        /// The question.
        question: String,
        /// Answer choices; empty means free-form.
        options: Vec<String>,
    },
    /// The turn hit its iteration cap. Interactive: see the unknown-types rule.
    LoopCapRequest {
        /// The request's id.
        id: String,
        /// The cap.
        cap: u32,
        /// Tools called most recently.
        recent_tools: Vec<String>,
    },
    /// A call was blocked by trust mode and not run.
    ActionBlocked {
        /// Tool name.
        tool_name: String,
        /// What the call would have done.
        detail: String,
        /// Diff preview, if there is one.
        preview: Option<DiffPreview>,
    },

    /// Context use, from Koda's estimate or the provider's count.
    ContextUsage {
        /// Tokens in use.
        used: usize,
        /// The window.
        max: usize,
    },
    /// The server's context window turned out to differ from Koda's.
    ContextWindowChanged {
        /// The server's window in tokens.
        tokens: usize,
    },
    /// Context was compacted, or a compaction was tried. `started` is
    /// followed by exactly one other status; `window` and `idle` have no
    /// `started`.
    Compaction {
        /// What set it off.
        trigger: CompactionTrigger,
        /// How far it got.
        #[serde(flatten)]
        status: CompactionStatus,
    },
    /// A model call will be tried again.
    Retry {
        /// Why.
        reason: RetryReason,
        /// The attempt that just failed, counting from 1.
        attempt: u32,
        /// How many attempts the call gets in total.
        max_attempts: u32,
        /// The wait before the next attempt.
        delay_ms: u64,
    },
    /// The loop guard saw the model repeating itself.
    LoopGuard {
        /// What it did.
        action: LoopGuardAction,
        /// What was repeated.
        detail: String,
    },

    /// A note for people. Don't parse it.
    Info {
        /// The text.
        message: String,
    },
    /// A warning for people. Don't parse it.
    Warn {
        /// The text.
        message: String,
    },
    /// An error for people. Don't parse it.
    Error {
        /// The text.
        message: String,
    },

    /// What the turn used, however it ended. Comes just before `turn_end`.
    /// Counts this turn only, including sub-agents and compaction.
    TurnSummary {
        /// Tokens and model calls.
        usage: UsageTotals,
        /// Wall-clock time of the turn.
        elapsed_ms: u64,
    },
    /// The turn is over, after any retries. Exactly one per turn.
    TurnEnd {
        /// How it ended.
        reason: TurnEndReason,
    },

    /// `koda -p`'s last line, and all of `--output-format json`.
    RunSummary(RunSummary),
}

/// The run as a whole.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunSummary {
    /// Whether the run succeeded. A cancelled turn is not a failure.
    pub success: bool,
    /// Why it failed.
    pub error: Option<String>,
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// The last reply.
    pub response: String,
    /// How the turn ended. `None` if it never did.
    pub turn_end: Option<TurnEndReason>,
    /// Wall-clock time of the run.
    pub elapsed_ms: u64,
    /// Everything stored for this session, earlier turns of a resumed session
    /// included. Sub-agents keep their own sessions and aren't counted here.
    pub usage: UsageTotals,
    /// `usage`, split by agent.
    pub usage_by_agent: BTreeMap<String, UsageTotals>,
    /// Top-level tool calls by tool name.
    pub tool_calls: BTreeMap<String, u64>,
    /// Sub-agent tool calls by tool name.
    pub sub_agent_tool_calls: BTreeMap<String, u64>,
}

/// What an engine event becomes on the wire. `None` for engine-only events.
pub fn project(event: EngineEvent) -> Option<WireEvent> {
    Some(match event {
        EngineEvent::TurnStart { .. } => WireEvent::TurnStart,
        EngineEvent::TextDelta { text } => WireEvent::TextDelta { text },
        EngineEvent::TextDone => WireEvent::TextDone,
        EngineEvent::ThinkingStart => WireEvent::ThinkingStart,
        EngineEvent::ThinkingDelta { text } => WireEvent::ThinkingDelta { text },
        EngineEvent::ThinkingDone => WireEvent::ThinkingDone,
        EngineEvent::ToolCallStart { id, name, args, .. } => {
            WireEvent::ToolCallStart { id, name, args }
        }
        EngineEvent::ToolOutputLine {
            id,
            line,
            is_stderr,
        } => WireEvent::ToolOutputLine {
            id,
            line,
            is_stderr,
        },
        EngineEvent::ToolCallResult {
            id,
            name,
            output,
            outcome,
        } => WireEvent::ToolCallResult {
            id,
            name,
            output,
            outcome,
        },
        EngineEvent::SubAgentStart { agent_name } => WireEvent::SubAgentStart { agent_name },
        EngineEvent::TodoUpdate { items, diff } => WireEvent::TodoUpdate { items, diff },
        EngineEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            preview,
            effect,
        } => WireEvent::ApprovalRequest {
            id,
            tool_name,
            detail,
            preview,
            effect,
        },
        EngineEvent::AskUserRequest {
            id,
            question,
            options,
        } => WireEvent::AskUserRequest {
            id,
            question,
            options,
        },
        EngineEvent::LoopCapReached {
            id,
            cap,
            recent_tools,
        } => WireEvent::LoopCapRequest {
            id,
            cap,
            recent_tools,
        },
        EngineEvent::ActionBlocked {
            tool_name,
            detail,
            preview,
        } => WireEvent::ActionBlocked {
            tool_name,
            detail,
            preview,
        },
        EngineEvent::ContextUsage { used, max } => WireEvent::ContextUsage { used, max },
        EngineEvent::ContextWindowChanged { tokens } => WireEvent::ContextWindowChanged { tokens },
        EngineEvent::Compaction {
            trigger, status, ..
        } => WireEvent::Compaction { trigger, status },
        EngineEvent::Retry {
            reason,
            attempt,
            max_attempts,
            delay_ms,
            ..
        } => WireEvent::Retry {
            reason,
            attempt,
            max_attempts,
            delay_ms,
        },
        EngineEvent::LoopGuard { action, detail, .. } => WireEvent::LoopGuard { action, detail },
        EngineEvent::Info { message } => WireEvent::Info { message },
        EngineEvent::Warn { message } => WireEvent::Warn { message },
        EngineEvent::Error { message } => WireEvent::Error { message },
        EngineEvent::TurnSummary {
            usage, elapsed_ms, ..
        } => WireEvent::TurnSummary { usage, elapsed_ms },
        EngineEvent::TurnEnd { reason, .. } => WireEvent::TurnEnd { reason },

        // Presentation hints, and the TUI's own live feed: a sub-agent's work
        // is on the wire as its own lines, with `parent_id`.
        EngineEvent::ResponseStart
        | EngineEvent::SpinnerStart { .. }
        | EngineEvent::SpinnerStop
        | EngineEvent::StatusUpdate { .. }
        | EngineEvent::Footer { .. }
        | EngineEvent::ChildTaskUpdate { .. }
        | EngineEvent::ChildAgentActivity { .. }
        | EngineEvent::ModelUsage { .. } => return None,
    })
}

/// Turns one session's engine events into lines. Adds the ids [`project`]
/// doesn't know: the session it was given and the turn in progress.
#[derive(Debug)]
pub struct WireStream {
    session_id: String,
    turn_id: Mutex<Option<String>>,
}

impl WireStream {
    /// A stream for this session.
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            turn_id: Mutex::new(None),
        }
    }

    /// The line for an engine event, if it has one.
    pub fn line(&self, parent_id: Option<&str>, event: EngineEvent) -> Option<WireLine> {
        let mut current = self.turn_id.lock().unwrap_or_else(PoisonError::into_inner);
        let turn_id = match &event {
            EngineEvent::TurnStart { turn_id } => {
                *current = Some(turn_id.clone());
                current.clone()
            }
            EngineEvent::TurnEnd { .. } => current.take(),
            _ => current.clone(),
        };
        Some(WireLine {
            turn_id,
            parent_id: parent_id.map(str::to_string),
            ..WireLine::for_session(&self.session_id, project(event)?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/src/wire-schema.json");

    /// Fails when `WireLine` changes and the checked-in schema doesn't.
    #[test]
    fn schema_file_is_current() {
        let schema = serde_json::to_value(schemars::schema_for!(WireLine)).unwrap();
        if std::env::var_os("KODA_UPDATE_WIRE_SCHEMA").is_some() {
            let text = serde_json::to_string_pretty(&schema).unwrap();
            std::fs::write(SCHEMA_PATH, text + "\n").unwrap();
            return;
        }
        let text = std::fs::read_to_string(SCHEMA_PATH).unwrap();
        let checked_in: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            checked_in, schema,
            "docs/src/wire-schema.json is stale. Refresh it with \
             KODA_UPDATE_WIRE_SCHEMA=1 cargo test -p koda-core --lib engine::wire"
        );
    }

    #[test]
    fn hello_comes_before_any_session() {
        let line = serde_json::to_value(WireLine::hello(ONE_SHOT_CAPABILITIES)).unwrap();
        assert_eq!(line["type"], "hello");
        assert_eq!(line["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(line["koda_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(line["capabilities"], json!(["one_shot", "self_answer"]));
        assert!(line.get("session_id").is_none());
    }

    fn line(stream: &WireStream, parent_id: Option<&str>, event: EngineEvent) -> Value {
        serde_json::to_value(stream.line(parent_id, event).unwrap()).unwrap()
    }

    #[test]
    fn ids_follow_the_session_and_turn() {
        let stream = WireStream::new("s1");
        let info = |message: &str| EngineEvent::Info {
            message: message.into(),
        };

        let before = line(&stream, None, info("before"));
        assert_eq!(before["session_id"], "s1");
        assert!(before.get("turn_id").is_none());

        let start = EngineEvent::TurnStart {
            turn_id: "t1".into(),
        };
        assert_eq!(
            line(&stream, None, start),
            json!({"session_id": "s1", "turn_id": "t1", "type": "turn_start"})
        );

        let child = line(&stream, Some("call-1"), info("inside"));
        assert_eq!(child["turn_id"], "t1");
        assert_eq!(child["parent_id"], "call-1");

        let end = EngineEvent::TurnEnd {
            turn_id: "t1".into(),
            reason: TurnEndReason::Cancelled,
        };
        let end = line(&stream, None, end);
        assert_eq!(end["turn_id"], "t1");
        assert_eq!(end["reason"]["kind"], "cancelled");
        assert!(end.get("parent_id").is_none());

        assert!(line(&stream, None, info("after")).get("turn_id").is_none());
    }

    #[test]
    fn presentation_stays_off_the_wire() {
        for event in [
            EngineEvent::ResponseStart,
            EngineEvent::SpinnerStop,
            EngineEvent::SpinnerStart {
                message: "Thinking...".into(),
            },
            EngineEvent::StatusUpdate {
                model: "m".into(),
                provider: "p".into(),
                context_pct: 0.5,
                approval_mode: "safe".into(),
                active_tools: 0,
            },
            EngineEvent::Footer {
                prompt_tokens: 1,
                completion_tokens: 1,
                cache_read_tokens: 0,
                thinking_tokens: 0,
                total_chars: 1,
                elapsed_ms: 1,
                rate: 1.0,
                context: "1%".into(),
            },
            EngineEvent::ModelUsage {
                usage: UsageTotals::default(),
            },
        ] {
            assert!(project(event).is_none());
        }
    }

    #[test]
    fn typed_state_has_no_prose() {
        let line = serde_json::to_value(
            project(EngineEvent::Compaction {
                trigger: CompactionTrigger::Threshold,
                status: CompactionStatus::Completed {
                    messages_removed: 12,
                    summary_tokens: Some(300),
                },
                notice: Some(super::super::event::Notice::Info("✅ Compacted".into())),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            line,
            json!({
                "type": "compaction",
                "trigger": "threshold",
                "status": "completed",
                "messages_removed": 12,
                "summary_tokens": 300,
            })
        );
    }

    #[test]
    fn a_tool_result_says_how_it_went() {
        let line = serde_json::to_value(
            project(EngineEvent::ToolCallResult {
                id: "a".into(),
                name: "Bash".into(),
                output: "Exit code: 1".into(),
                outcome: ToolOutcome::Error,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(line["outcome"], "error");
    }

    #[test]
    fn a_sub_agent_tool_call_has_no_bool() {
        let line = serde_json::to_value(
            project(EngineEvent::ToolCallStart {
                id: "a".into(),
                name: "Read".into(),
                args: json!({}),
                is_sub_agent: true,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(line.get("is_sub_agent").is_none());
    }
}
