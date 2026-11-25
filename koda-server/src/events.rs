//! What a session's engine events become on the protocol.
//!
//! The engine's [`EngineEvent`] stays internal and can change freely; this
//! is where each one gets its protocol form, or none. Engine events are
//! flat (a text chunk, a tool call starting); the protocol has items that
//! start, stream and complete, so [`EventProjector`] keeps the state that
//! takes: the turn in progress, the open message and reasoning items, the
//! open tool calls, and the server requests waiting for an answer.
//!
//! Not on the protocol: spinners, the status bar and the footer are
//! presentation; the session's own `Info` lines (tools run in parallel,
//! steering text injected) say what the items already show. A sub-agent's
//! own turn events are folded into its parent's tool call, and its notices,
//! like a skill's change of tool scope, are that call's progress.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use koda_core::engine::event::{
    CompactionStatus as EngineCompactionStatus, CompactionTrigger as EngineCompactionTrigger,
    LoopGuardAction as EngineLoopGuardAction, Notice, RetryReason as EngineRetryReason,
    ToolOutcome, UsageTotals,
};
use koda_core::engine::{EngineEvent, TurnEndReason};
use koda_core::preview::DiffPreview;
use koda_protocol::item::{
    CompactionStatus, CompactionTrigger, FileUpdateChange, Item, ItemCompletedNotification,
    ItemStartedNotification, OutputStream, PatchChangeKind, TextDeltaNotification,
    ToolCallApprovalParams, ToolCallOutputDeltaNotification, ToolCallProgressNotification,
    ToolCallStatus, ToolEffect, ToolKind, UserInputParams,
};
use koda_protocol::rpc::RequestId;
use koda_protocol::server::{
    ErrorNotification, Retry, RetryReason, ServerRequestResolvedNotification, WarningNotification,
};
use koda_protocol::session::{
    SessionTokenUsageUpdatedNotification, TokenUsage, TokenUsageBreakdown,
};
use koda_protocol::turn::{
    LoopCapParams, LoopGuardAction, PlanStep, PlanStepStatus, Turn, TurnCompletedNotification,
    TurnError, TurnLoopGuardNotification, TurnPlanUpdatedNotification, TurnStartedNotification,
    TurnStatus, Usage,
};
use koda_protocol::{ServerNotification, ServerRequest};
use serde_json::Value;

/// One message for the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    /// Something happened.
    Notification(ServerNotification),
    /// Koda needs an answer.
    Request(ServerRequest),
}

impl Outgoing {
    /// The message as one JSON-RPC line.
    pub fn to_json(&self) -> serde_json::Result<String> {
        match self {
            Self::Notification(n) => serde_json::to_string(&n.to_rpc()?),
            Self::Request(r) => serde_json::to_string(&r.to_rpc()?),
        }
    }

    /// Whether this is a delta: more text or output for an item already
    /// started. `koda exec --json` leaves them out unless asked.
    pub fn is_delta(&self) -> bool {
        matches!(
            self,
            Self::Notification(
                ServerNotification::AgentMessageDelta(_)
                    | ServerNotification::ReasoningTextDelta(_)
                    | ServerNotification::ToolCallOutputDelta(_)
            )
        )
    }
}

/// A tool call that started and hasn't completed.
#[derive(Debug)]
struct OpenCall {
    tool: String,
    arguments: Value,
    effect: ToolEffect,
    parent: Option<String>,
    /// Set when Koda asked about the call or blocked it.
    detail: Option<String>,
    /// The file changes Koda showed when it asked or blocked.
    changes: Vec<FileUpdateChange>,
    /// The notices sent about it.
    progress: Vec<String>,
}

/// An open message or reasoning item.
#[derive(Debug)]
struct OpenText {
    id: String,
    text: String,
}

/// An open compaction.
#[derive(Debug)]
struct OpenCompaction {
    id: String,
    trigger: CompactionTrigger,
    context_percent: Option<u64>,
}

/// Turns one session's engine events into protocol messages.
#[derive(Debug)]
pub struct EventProjector {
    session_id: String,
    turn_id: Option<String>,
    usage: Option<(Usage, u64)>,
    message: HashMap<Option<String>, OpenText>,
    reasoning: HashMap<Option<String>, OpenText>,
    calls: HashMap<String, OpenCall>,
    compaction: Option<OpenCompaction>,
    /// Engine request id to protocol request id, until answered.
    requests: HashMap<String, RequestId>,
    next_item: u64,
    next_request: i64,
    /// Request ids shared with the host's other sessions, so a response
    /// names one session's request. `None` numbers them per projector.
    shared_request_ids: Option<Arc<AtomicI64>>,
    /// The session's token counts since it was created.
    tokens: TokenUsage,
    /// Each running sub-agent's name, by the `InvokeAgent` call it runs
    /// under, from its `SubAgentStart`: parallel calls start after their
    /// sub-agents have run, so the call can't say whose usage it is.
    agent_names: HashMap<String, String>,
    /// The session's agent, whose tools (MCP ones included) classify each
    /// call's effect; without one, the built-in tools'.
    agent: Option<SessionAgent>,
}

/// The session's agent, for classifying its calls. Shown by name only.
#[derive(Clone)]
struct SessionAgent(Arc<koda_core::agent::KodaAgent>);

impl std::fmt::Debug for SessionAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionAgent")
    }
}

impl EventProjector {
    /// A projector for this session.
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            turn_id: None,
            usage: None,
            message: HashMap::new(),
            reasoning: HashMap::new(),
            calls: HashMap::new(),
            compaction: None,
            requests: HashMap::new(),
            next_item: 0,
            next_request: 0,
            shared_request_ids: None,
            tokens: TokenUsage::default(),
            agent_names: HashMap::new(),
            agent: None,
        }
    }

    /// A projector whose server requests take their ids from `ids`, which
    /// the host's other sessions share.
    pub fn with_request_ids(session_id: &str, ids: Arc<AtomicI64>) -> Self {
        Self {
            shared_request_ids: Some(ids),
            ..Self::new(session_id)
        }
    }

    /// Start the session's counts from what its stored calls used, by
    /// agent, as Codex restores a resumed thread's: `by_agent` as the
    /// protocol counts it, and the context window it runs with. Returns the
    /// counts, for the client that resumed it.
    pub fn restore_usage(
        &mut self,
        by_agent: std::collections::BTreeMap<String, Usage>,
        context_window: Option<u64>,
    ) -> TokenUsage {
        let mut total = TokenUsageBreakdown::default();
        for usage in by_agent.values() {
            add(
                &mut total,
                &TokenUsageBreakdown {
                    total_tokens: usage.input_tokens + usage.output_tokens,
                    input_tokens: usage.input_tokens,
                    cached_input_tokens: usage.cached_input_tokens,
                    cache_write_input_tokens: usage.cache_creation_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_output_tokens: usage.reasoning_tokens,
                },
            );
        }
        self.tokens = TokenUsage {
            total,
            last: TokenUsageBreakdown::default(),
            model_context_window: context_window,
            by_agent,
        };
        self.tokens.clone()
    }

    /// Whose model call this is: `main` for the session's own, else the
    /// name of the sub-agent whose `InvokeAgent` call `parent` is.
    fn agent_of(&self, parent: Option<&str>) -> String {
        let Some(parent) = parent else {
            return "main".to_string();
        };
        self.agent_names
            .get(parent)
            .map(String::as_str)
            .or_else(|| {
                self.calls
                    .get(parent)
                    .and_then(|call| call.arguments.get("agent_name"))
                    .and_then(Value::as_str)
            })
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("main")
            .to_string()
    }

    /// This projector, classifying tool calls with `agent`'s tools, as its
    /// approvals are.
    pub fn with_agent(self, agent: Arc<koda_core::agent::KodaAgent>) -> Self {
        Self {
            agent: Some(SessionAgent(agent)),
            ..self
        }
    }

    /// What a call of `name` with `arguments` does.
    fn effect_of_call(&self, name: &str, arguments: &Value) -> ToolEffect {
        let effect = match &self.agent {
            Some(SessionAgent(agent)) => agent.tools.catalog().classify_call(name, arguments),
            None => koda_core::tools::ToolCatalog::default_static().classify_call(name, arguments),
        };
        effect_of(effect)
    }

    /// The engine's id for the open request `request_id`, if this
    /// projector sent it and it is unanswered.
    pub fn engine_request(&self, request_id: &RequestId) -> Option<String> {
        self.requests
            .iter()
            .find(|(_, id)| *id == request_id)
            .map(|(engine_id, _)| engine_id.clone())
    }

    /// `serverRequest/resolved` for every request still open, which then
    /// needs no answer: the turn they belonged to is over.
    pub fn resolve_all(&mut self) -> Vec<Outgoing> {
        let mut open: Vec<_> = self.requests.keys().cloned().collect();
        open.sort();
        open.iter()
            .filter_map(|engine_id| self.resolved(engine_id))
            .collect()
    }

    /// A `userMessage` item, started and completed: what the user said,
    /// with the paths it mentioned.
    pub fn user_message(
        &mut self,
        text: String,
        client_message_ids: Vec<String>,
        mentions: Vec<koda_protocol::turn::Mention>,
    ) -> Vec<Outgoing> {
        let item = Item::UserMessage {
            id: self.new_item_id("user"),
            text,
            client_message_ids,
            mentions,
        };
        vec![
            self.item_started(None, item.clone()),
            self.item_completed(None, item),
        ]
    }

    /// The session's id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// `serverRequest/resolved` for the engine request `engine_id`, once it
    /// is answered. `None` for one this projector never sent.
    pub fn resolved(&mut self, engine_id: &str) -> Option<Outgoing> {
        let request_id = self.requests.remove(engine_id)?;
        Some(Outgoing::Notification(
            ServerNotification::ServerRequestResolved(ServerRequestResolvedNotification {
                session_id: self.session_id.clone(),
                request_id,
            }),
        ))
    }

    /// The messages one engine event becomes. `parent` is the tool call
    /// that started the sub-agent the event came from, if one did.
    pub fn project(&mut self, parent: Option<&str>, event: EngineEvent) -> Vec<Outgoing> {
        let parent = parent.map(str::to_string);
        let mut out = Vec::new();
        match event {
            // A sub-agent's turn is its parent's tool call.
            EngineEvent::TurnStart { .. }
            | EngineEvent::TurnSummary { .. }
            | EngineEvent::TurnEnd { .. }
                if parent.is_some() => {}
            EngineEvent::TurnStart { turn_id } => {
                self.turn_id = Some(turn_id.clone());
                self.usage = None;
                out.push(notification(ServerNotification::TurnStarted(
                    TurnStartedNotification {
                        session_id: self.session_id.clone(),
                        turn: Turn {
                            id: turn_id,
                            status: TurnStatus::InProgress,
                            error: None,
                            usage: None,
                            duration_ms: None,
                            items: Vec::new(),
                        },
                    },
                )));
            }
            EngineEvent::TurnSummary {
                usage, elapsed_ms, ..
            } => self.usage = Some((usage_of(&usage), elapsed_ms)),
            EngineEvent::TurnEnd { turn_id, reason } => {
                self.close_all(&mut out);
                let (status, error) = match reason {
                    TurnEndReason::Complete => (TurnStatus::Completed, None),
                    TurnEndReason::Cancelled => (TurnStatus::Interrupted, None),
                    TurnEndReason::Error { message } => (
                        TurnStatus::Failed,
                        Some(TurnError {
                            message,
                            code: None,
                        }),
                    ),
                    TurnEndReason::BudgetSpent { message } => (
                        TurnStatus::Failed,
                        Some(TurnError {
                            message,
                            code: Some(koda_protocol::turn::TurnErrorCode::TokenBudgetSpent),
                        }),
                    ),
                    // A reason added later reads as a failure until mapped.
                    _ => (TurnStatus::Failed, None),
                };
                let (usage, duration_ms) = match self.usage.take() {
                    Some((usage, ms)) => (Some(usage), Some(ms)),
                    None => (None, None),
                };
                out.push(notification(ServerNotification::TurnCompleted(
                    TurnCompletedNotification {
                        session_id: self.session_id.clone(),
                        turn: Turn {
                            id: turn_id,
                            status,
                            error,
                            usage,
                            duration_ms,
                            items: Vec::new(),
                        },
                    },
                )));
                self.turn_id = None;
            }

            EngineEvent::TextDelta { text } => {
                if text.is_empty() {
                    return out;
                }
                let item_id = self.open_text(&mut out, &parent, TextKind::Message);
                let open = self.message.get_mut(&parent).expect("just opened");
                open.text.push_str(&text);
                out.push(notification(ServerNotification::AgentMessageDelta(
                    self.text_delta(item_id, text),
                )));
            }
            EngineEvent::TextDone => self.close_text(&mut out, &parent, TextKind::Message),
            EngineEvent::ThinkingStart => {
                self.open_text(&mut out, &parent, TextKind::Reasoning);
            }
            EngineEvent::ThinkingDelta { text } => {
                let item_id = self.open_text(&mut out, &parent, TextKind::Reasoning);
                let open = self.reasoning.get_mut(&parent).expect("just opened");
                open.text.push_str(&text);
                out.push(notification(ServerNotification::ReasoningTextDelta(
                    self.text_delta(item_id, text),
                )));
            }
            EngineEvent::ThinkingDone => self.close_text(&mut out, &parent, TextKind::Reasoning),

            EngineEvent::ToolCallStart {
                id,
                name,
                args,
                preview,
                ..
            } => {
                // Text before a call is finished when the call starts.
                self.close_text(&mut out, &parent, TextKind::Reasoning);
                self.close_text(&mut out, &parent, TextKind::Message);
                let effect = self.effect_of_call(&name, &args);
                // An edit's changes from its start, asked about or not, as
                // Codex's `fileChange` item carries them.
                let changes = changes_of(preview.as_ref());
                let item = Item::ToolCall {
                    id: id.clone(),
                    tool: name.clone(),
                    arguments: args.clone(),
                    effect: Some(effect),
                    kind: Some(ToolKind::of(&name)),
                    status: ToolCallStatus::InProgress,
                    output: None,
                    exit_code: None,
                    duration_ms: None,
                    detail: None,
                    changes: changes.clone(),
                    progress: Vec::new(),
                };
                self.calls.insert(
                    id,
                    OpenCall {
                        tool: name,
                        arguments: args,
                        effect,
                        parent: parent.clone(),
                        detail: None,
                        changes,
                        progress: Vec::new(),
                    },
                );
                out.push(self.item_started(parent, item));
            }
            EngineEvent::ToolOutputLine {
                id,
                line,
                is_stderr,
            } => out.push(notification(ServerNotification::ToolCallOutputDelta(
                ToolCallOutputDeltaNotification {
                    session_id: self.session_id.clone(),
                    turn_id: self.turn(),
                    item_id: id,
                    delta: format!("{line}\n"),
                    stream: if is_stderr {
                        OutputStream::Stderr
                    } else {
                        OutputStream::Stdout
                    },
                },
            ))),
            EngineEvent::ToolCallResult {
                id,
                name,
                output,
                outcome,
                exit_code,
                duration_ms,
            } => {
                self.agent_names.remove(&id);
                let call = self.calls.remove(&id);
                let (tool, arguments, effect, call_parent, detail, changes, progress) = match call {
                    Some(c) => (
                        c.tool,
                        c.arguments,
                        Some(c.effect),
                        c.parent,
                        c.detail,
                        c.changes,
                        c.progress,
                    ),
                    None => (
                        name,
                        Value::Null,
                        None,
                        parent,
                        None,
                        Vec::new(),
                        Vec::new(),
                    ),
                };
                let item = Item::ToolCall {
                    id,
                    kind: Some(ToolKind::of(&tool)),
                    tool,
                    arguments,
                    effect,
                    status: status_of(outcome),
                    output: Some(output),
                    exit_code,
                    duration_ms,
                    detail,
                    changes,
                    progress,
                };
                out.push(self.item_completed(call_parent, item));
            }
            EngineEvent::ActionBlocked {
                tool_call_id,
                detail,
                preview,
                ..
            } => {
                if let Some(call) = self.calls.get_mut(&tool_call_id) {
                    call.detail = Some(detail);
                    call.changes = changes_of(preview.as_ref());
                }
            }
            EngineEvent::ApprovalRequest {
                id,
                tool_call_id,
                tool_name,
                detail,
                preview,
                effect,
                allow_for_session,
            } => {
                let changes = changes_of(preview.as_ref());
                if let Some(call) = self.calls.get_mut(&tool_call_id) {
                    call.detail = Some(detail.clone());
                    call.changes = changes.clone();
                }
                let request_id = self.request_id(id);
                out.push(Outgoing::Request(ServerRequest::ToolCallApproval {
                    id: request_id,
                    params: ToolCallApprovalParams {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        item_id: tool_call_id,
                        tool: tool_name,
                        detail,
                        changes,
                        effect: effect_of(effect),
                        allow_for_session,
                        agent: None,
                    },
                }));
            }
            EngineEvent::AskUserRequest {
                id,
                tool_call_id,
                question,
                options,
            } => {
                let request_id = self.request_id(id);
                out.push(Outgoing::Request(ServerRequest::UserInput {
                    id: request_id,
                    params: UserInputParams {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        item_id: tool_call_id,
                        question,
                        options,
                    },
                }));
            }
            EngineEvent::LoopCapReached {
                id,
                cap,
                recent_tools,
            } => {
                let request_id = self.request_id(id);
                out.push(Outgoing::Request(ServerRequest::LoopCap {
                    id: request_id,
                    params: LoopCapParams {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        cap,
                        recent_tools,
                    },
                }));
            }

            EngineEvent::GoalUpdated { goal } => {
                out.push(notification(ServerNotification::SessionGoalUpdated(
                    koda_protocol::goal::SessionGoalUpdatedNotification {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn_id.clone(),
                        goal: crate::goal::to_wire(&goal),
                    },
                )));
            }
            EngineEvent::BackgroundExited { exits } => {
                use koda_protocol::background::{
                    BackgroundTerminalExit, BackgroundTerminalsExitedNotification,
                };
                out.push(notification(
                    ServerNotification::SessionBackgroundTerminalsExited(
                        BackgroundTerminalsExitedNotification {
                            session_id: self.session_id.clone(),
                            turn_id: self.turn(),
                            data: exits
                                .into_iter()
                                .map(|(pid, command, exit_code)| BackgroundTerminalExit {
                                    process_id: pid.to_string(),
                                    command,
                                    exit_code,
                                })
                                .collect(),
                        },
                    ),
                ));
            }
            EngineEvent::GoalJudged { verdict, reason } => {
                use koda_core::goal::GoalVerdict;
                use koda_protocol::goal::GoalVerdict as Wire;
                out.push(notification(ServerNotification::SessionGoalJudged(
                    koda_protocol::goal::SessionGoalJudgedNotification {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        verdict: match verdict {
                            GoalVerdict::Met => Wire::Met,
                            GoalVerdict::NotYet => Wire::NotYet,
                            GoalVerdict::Impossible => Wire::Impossible,
                        },
                        reason,
                    },
                )));
            }
            EngineEvent::TodoUpdate { items, .. } => {
                out.push(notification(ServerNotification::TurnPlanUpdated(
                    TurnPlanUpdatedNotification {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        plan: items
                            .into_iter()
                            .map(|item| PlanStep {
                                step: item.content,
                                status: match item.status {
                                    koda_core::tools::todo::TodoStatus::Pending => {
                                        PlanStepStatus::Pending
                                    }
                                    koda_core::tools::todo::TodoStatus::InProgress => {
                                        PlanStepStatus::InProgress
                                    }
                                    koda_core::tools::todo::TodoStatus::Completed => {
                                        PlanStepStatus::Completed
                                    }
                                },
                            })
                            .collect(),
                    },
                )));
            }
            EngineEvent::Compaction {
                trigger, status, ..
            } => self.compaction(&mut out, parent, trigger, status),
            EngineEvent::Retry {
                reason,
                attempt,
                max_attempts,
                delay_ms,
                notice,
            } => {
                // What was shown of a failed reply is gone; its item ends.
                if reason == EngineRetryReason::StreamFailed {
                    self.close_text(&mut out, &parent, TextKind::Reasoning);
                    self.close_text(&mut out, &parent, TextKind::Message);
                }
                let message = match notice {
                    Some(Notice::Info(text) | Notice::Warn(text)) => text,
                    None => format!("model call failed (attempt {attempt} of {max_attempts})"),
                };
                out.push(notification(ServerNotification::Error(ErrorNotification {
                    session_id: self.session_id.clone(),
                    turn_id: self.turn_id.clone(),
                    error: TurnError {
                        message,
                        code: None,
                    },
                    will_retry: true,
                    retry: Some(Retry {
                        reason: retry_reason_of(reason),
                        attempt,
                        max_attempts,
                        delay_ms,
                    }),
                })));
            }
            EngineEvent::LoopGuard { action, detail, .. } => {
                out.push(notification(ServerNotification::TurnLoopGuard(
                    TurnLoopGuardNotification {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        action: match action {
                            EngineLoopGuardAction::Feedback => LoopGuardAction::Feedback,
                            EngineLoopGuardAction::Stop => LoopGuardAction::Stop,
                        },
                        detail,
                    },
                )));
            }
            // A notice about a call: its sub-agent's, or a skill's scope.
            EngineEvent::Info { message } => {
                if let Some(item_id) = parent {
                    let message = message.trim().to_string();
                    if let Some(call) = self.calls.get_mut(&item_id) {
                        call.progress.push(message.clone());
                    }
                    out.push(notification(ServerNotification::ToolCallProgress(
                        ToolCallProgressNotification {
                            session_id: self.session_id.clone(),
                            turn_id: self.turn(),
                            item_id,
                            message,
                        },
                    )));
                }
            }
            // Sent once a call has returned, as Codex sends its token count:
            // only then does the provider say what it used.
            EngineEvent::ModelUsage { usage } => {
                let call = breakdown_of(&usage);
                add(&mut self.tokens.total, &call);
                let agent = self.agent_of(parent.as_deref());
                add_usage(
                    self.tokens.by_agent.entry(agent).or_default(),
                    &usage_of(&UsageTotals {
                        api_calls: 1,
                        ..usage
                    }),
                );
                if parent.is_none() {
                    self.tokens.last = call;
                }
                out.push(notification(ServerNotification::SessionTokenUsageUpdated(
                    SessionTokenUsageUpdatedNotification {
                        session_id: self.session_id.clone(),
                        turn_id: self.turn(),
                        token_usage: self.tokens.clone(),
                    },
                )));
            }
            // Koda's own count before a call isn't sent; the window is.
            EngineEvent::ContextUsage { max, .. } if parent.is_none() => {
                self.tokens.model_context_window = Some(max as u64);
            }
            EngineEvent::Warn { message } => {
                out.push(notification(ServerNotification::Warning(
                    WarningNotification {
                        session_id: Some(self.session_id.clone()),
                        message,
                    },
                )));
            }
            EngineEvent::Error { message } => {
                out.push(notification(ServerNotification::Error(ErrorNotification {
                    session_id: self.session_id.clone(),
                    turn_id: self.turn_id.clone(),
                    error: TurnError {
                        message,
                        code: None,
                    },
                    will_retry: false,
                    retry: None,
                })));
            }

            // Not sent: whose usage a sub-agent's model calls are.
            EngineEvent::SubAgentStart { agent_name } => {
                if let Some(call) = parent {
                    self.agent_names.insert(call, agent_name);
                }
            }

            // Presentation, state a client reads, and the TUI's own feeds.
            // A new engine event lands here until it is given a form.
            _ => {}
        }
        out
    }

    fn turn(&self) -> String {
        self.turn_id.clone().unwrap_or_default()
    }

    /// A sub-agent's approval request, with an id of the host's own: it is
    /// answered on its own reply, not through the turn.
    pub(crate) fn agent_approval(
        &mut self,
        request: koda_core::approval_flow::AgentApproval,
    ) -> (RequestId, ToolCallApprovalParams) {
        let id = self.new_request_id();
        let params = ToolCallApprovalParams {
            session_id: self.session_id.clone(),
            turn_id: self.turn(),
            item_id: request.tool_call_id,
            tool: request.tool_name,
            detail: request.detail,
            changes: changes_of(request.preview.as_ref()),
            effect: effect_of(request.effect),
            allow_for_session: request.allow_for_session,
            agent: Some(koda_protocol::item::ApprovalAgent {
                name: request.agent_name,
                id: request.agent_id,
            }),
        };
        (id, params)
    }

    /// A request id for a request of the host's own, not the engine's: an
    /// MCP server's elicitation.
    pub(crate) fn new_request_id(&mut self) -> RequestId {
        match &self.shared_request_ids {
            Some(ids) => RequestId::Integer(ids.fetch_add(1, Ordering::Relaxed) + 1),
            None => {
                self.next_request += 1;
                RequestId::Integer(self.next_request)
            }
        }
    }

    fn request_id(&mut self, engine_id: String) -> RequestId {
        let id = match &self.shared_request_ids {
            Some(ids) => RequestId::Integer(ids.fetch_add(1, Ordering::Relaxed) + 1),
            None => {
                self.next_request += 1;
                RequestId::Integer(self.next_request)
            }
        };
        self.requests.insert(engine_id, id.clone());
        id
    }

    fn new_item_id(&mut self, prefix: &str) -> String {
        self.next_item += 1;
        format!("{prefix}-{}", self.next_item)
    }

    fn text_delta(&self, item_id: String, delta: String) -> TextDeltaNotification {
        TextDeltaNotification {
            session_id: self.session_id.clone(),
            turn_id: self.turn(),
            item_id,
            delta,
        }
    }

    fn item_started(&self, parent: Option<String>, item: Item) -> Outgoing {
        notification(ServerNotification::ItemStarted(ItemStartedNotification {
            session_id: self.session_id.clone(),
            turn_id: self.turn(),
            parent_item_id: parent,
            item,
            started_at_ms: now_ms(),
        }))
    }

    fn item_completed(&self, parent: Option<String>, item: Item) -> Outgoing {
        notification(ServerNotification::ItemCompleted(
            ItemCompletedNotification {
                session_id: self.session_id.clone(),
                turn_id: self.turn(),
                parent_item_id: parent,
                item,
                completed_at_ms: now_ms(),
            },
        ))
    }

    /// The open item of `kind` for `parent`, started if there is none.
    fn open_text(
        &mut self,
        out: &mut Vec<Outgoing>,
        parent: &Option<String>,
        kind: TextKind,
    ) -> String {
        if let Some(open) = self.texts(kind).get(parent) {
            return open.id.clone();
        }
        let id = self.new_item_id(kind.prefix());
        out.push(self.item_started(parent.clone(), kind.item(id.clone(), String::new())));
        self.texts(kind).insert(
            parent.clone(),
            OpenText {
                id: id.clone(),
                text: String::new(),
            },
        );
        id
    }

    fn close_text(&mut self, out: &mut Vec<Outgoing>, parent: &Option<String>, kind: TextKind) {
        if let Some(open) = self.texts(kind).remove(parent) {
            out.push(self.item_completed(parent.clone(), kind.item(open.id, open.text)));
        }
    }

    fn texts(&mut self, kind: TextKind) -> &mut HashMap<Option<String>, OpenText> {
        match kind {
            TextKind::Message => &mut self.message,
            TextKind::Reasoning => &mut self.reasoning,
        }
    }

    fn compaction(
        &mut self,
        out: &mut Vec<Outgoing>,
        parent: Option<String>,
        trigger: EngineCompactionTrigger,
        status: EngineCompactionStatus,
    ) {
        let trigger = match trigger {
            EngineCompactionTrigger::Threshold => CompactionTrigger::Threshold,
            EngineCompactionTrigger::Overflow => CompactionTrigger::Overflow,
            EngineCompactionTrigger::Window => CompactionTrigger::Window,
            EngineCompactionTrigger::Manual => CompactionTrigger::Manual,
            EngineCompactionTrigger::PostTurn => CompactionTrigger::PostTurn,
        };
        let item =
            |open: &OpenCompaction, status, removed, tokens, reason| Item::ContextCompaction {
                id: open.id.clone(),
                trigger: open.trigger,
                status,
                context_percent: open.context_percent,
                messages_removed: removed,
                summary_tokens: tokens,
                reason,
            };
        if let EngineCompactionStatus::Started { context_percent } = status {
            let open = OpenCompaction {
                id: self.new_item_id("compaction"),
                trigger,
                context_percent: context_percent.map(|p| p as u64),
            };
            out.push(self.item_started(
                parent,
                item(&open, CompactionStatus::InProgress, None, None, None),
            ));
            self.compaction = Some(open);
            return;
        }
        // `window` and `idle` have no start: start and end together.
        let open = match self.compaction.take() {
            Some(open) => open,
            None => {
                let open = OpenCompaction {
                    id: self.new_item_id("compaction"),
                    trigger,
                    context_percent: None,
                };
                out.push(self.item_started(
                    parent.clone(),
                    item(&open, CompactionStatus::InProgress, None, None, None),
                ));
                open
            }
        };
        let done = match status {
            EngineCompactionStatus::Completed {
                messages_removed,
                summary_tokens,
            } => item(
                &open,
                CompactionStatus::Completed,
                Some(messages_removed as u64),
                summary_tokens.map(|t| t as u64),
                None,
            ),
            EngineCompactionStatus::Skipped { reason } => {
                item(&open, CompactionStatus::Skipped, None, None, Some(reason))
            }
            EngineCompactionStatus::Failed { error } => {
                item(&open, CompactionStatus::Failed, None, None, Some(error))
            }
            EngineCompactionStatus::Started { .. } => unreachable!("handled above"),
        };
        out.push(self.item_completed(parent, done));
    }

    /// Complete every open item: the turn ended.
    fn close_all(&mut self, out: &mut Vec<Outgoing>) {
        let parents: Vec<Option<String>> = self
            .reasoning
            .keys()
            .chain(self.message.keys())
            .cloned()
            .collect();
        for parent in parents {
            self.close_text(out, &parent, TextKind::Reasoning);
            self.close_text(out, &parent, TextKind::Message);
        }
        let mut open: Vec<(String, OpenCall)> = self.calls.drain().collect();
        open.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, call) in open {
            let item = Item::ToolCall {
                id,
                kind: Some(ToolKind::of(&call.tool)),
                tool: call.tool,
                arguments: call.arguments,
                effect: Some(call.effect),
                status: ToolCallStatus::Cancelled,
                output: None,
                exit_code: None,
                duration_ms: None,
                detail: call.detail,
                changes: call.changes,
                progress: call.progress,
            };
            out.push(self.item_completed(call.parent, item));
        }
        if let Some(open) = self.compaction.take() {
            out.push(self.item_completed(
                None,
                Item::ContextCompaction {
                    id: open.id,
                    trigger: open.trigger,
                    status: CompactionStatus::Failed,
                    context_percent: open.context_percent,
                    messages_removed: None,
                    summary_tokens: None,
                    reason: Some("the turn ended first".into()),
                },
            ));
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum TextKind {
    Message,
    Reasoning,
}

impl TextKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Message => "msg",
            Self::Reasoning => "reasoning",
        }
    }

    fn item(self, id: String, text: String) -> Item {
        match self {
            Self::Message => Item::AgentMessage { id, text },
            Self::Reasoning => Item::Reasoning { id, text },
        }
    }
}

fn notification(n: ServerNotification) -> Outgoing {
    Outgoing::Notification(n)
}

/// Engine usage on the protocol.
pub fn usage_of(usage: &UsageTotals) -> Usage {
    let n = |v: i64| v.max(0) as u64;
    Usage {
        input_tokens: n(usage.input_tokens()),
        cached_input_tokens: n(usage.cache_read_tokens),
        cache_creation_tokens: n(usage.cache_creation_tokens),
        output_tokens: n(usage.completion_tokens),
        reasoning_tokens: n(usage.thinking_tokens),
        model_calls: n(usage.api_calls),
        web_search_requests: n(usage.web_search_requests),
    }
}

fn status_of(outcome: ToolOutcome) -> ToolCallStatus {
    match outcome {
        ToolOutcome::Success => ToolCallStatus::Completed,
        ToolOutcome::Error => ToolCallStatus::Failed,
        ToolOutcome::Declined => ToolCallStatus::Declined,
        ToolOutcome::Cancelled => ToolCallStatus::Cancelled,
    }
}

fn effect_of(effect: koda_core::tools::ToolEffect) -> ToolEffect {
    use koda_core::tools::ToolEffect as E;
    match effect {
        E::ReadOnly => ToolEffect::ReadOnly,
        E::RemoteAction => ToolEffect::RemoteAction,
        E::LocalMutation => ToolEffect::LocalMutation,
        E::Destructive => ToolEffect::Destructive,
    }
}

fn retry_reason_of(reason: EngineRetryReason) -> RetryReason {
    match reason {
        EngineRetryReason::RateLimit => RetryReason::RateLimit,
        EngineRetryReason::Network => RetryReason::Network,
        EngineRetryReason::ServerError => RetryReason::ServerError,
        EngineRetryReason::StreamFailed => RetryReason::StreamFailed,
        EngineRetryReason::ContextOverflow => RetryReason::ContextOverflow,
        EngineRetryReason::EmptyResponse => RetryReason::EmptyResponse,
    }
}

/// The file changes a preview shows, in Codex's shape.
fn changes_of(preview: Option<&DiffPreview>) -> Vec<FileUpdateChange> {
    match preview {
        Some(DiffPreview::UnifiedDiff(diff)) => vec![FileUpdateChange {
            path: diff.path.clone(),
            kind: PatchChangeKind::Update { move_path: None },
            diff: diff.unified_text(),
        }],
        Some(DiffPreview::WriteNew(new)) => vec![FileUpdateChange {
            path: new.path.clone(),
            kind: PatchChangeKind::Add,
            diff: new.content.clone(),
        }],
        Some(DiffPreview::FileNotYetExists) | None => Vec::new(),
    }
}

/// One model call's tokens, in Codex's breakdown.
/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn breakdown_of(usage: &UsageTotals) -> TokenUsageBreakdown {
    let count = |n: i64| n.max(0) as u64;
    let input = count(usage.input_tokens());
    let output = count(usage.completion_tokens);
    TokenUsageBreakdown {
        total_tokens: input + output,
        input_tokens: input,
        cached_input_tokens: count(usage.cache_read_tokens),
        cache_write_input_tokens: count(usage.cache_creation_tokens),
        output_tokens: output,
        reasoning_output_tokens: count(usage.thinking_tokens),
    }
}

fn add_usage(total: &mut Usage, call: &Usage) {
    total.input_tokens += call.input_tokens;
    total.cached_input_tokens += call.cached_input_tokens;
    total.cache_creation_tokens += call.cache_creation_tokens;
    total.output_tokens += call.output_tokens;
    total.reasoning_tokens += call.reasoning_tokens;
    total.model_calls += call.model_calls;
    total.web_search_requests += call.web_search_requests;
}

fn add(total: &mut TokenUsageBreakdown, call: &TokenUsageBreakdown) {
    total.total_tokens += call.total_tokens;
    total.input_tokens += call.input_tokens;
    total.cached_input_tokens += call.cached_input_tokens;
    total.cache_write_input_tokens += call.cache_write_input_tokens;
    total.output_tokens += call.output_tokens;
    total.reasoning_output_tokens += call.reasoning_output_tokens;
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_core::engine::event::{CompactionStatus as CS, CompactionTrigger as CT};
    use serde_json::json;

    fn run(p: &mut EventProjector, events: Vec<(Option<&str>, EngineEvent)>) -> Vec<Value> {
        events
            .into_iter()
            .flat_map(|(parent, e)| p.project(parent, e))
            .map(|o| serde_json::from_str(&o.to_json().unwrap()).unwrap())
            .collect()
    }

    fn methods(lines: &[Value]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let m = l["method"].as_str().unwrap();
                match l["params"]["item"]["type"].as_str() {
                    Some(t) => format!("{m} {t}"),
                    None => m.to_string(),
                }
            })
            .collect()
    }

    /// Every item that starts completes, once, after it starts.
    fn balanced(lines: &[Value]) {
        let mut open = std::collections::HashSet::new();
        for l in lines {
            let id = l["params"]["item"]["id"].as_str().map(str::to_string);
            match (l["method"].as_str().unwrap(), id) {
                ("item/started", Some(id)) => assert!(open.insert(id.clone()), "{id} twice"),
                ("item/completed", Some(id)) => assert!(open.remove(&id), "{id} not open"),
                _ => {}
            }
        }
        assert!(open.is_empty(), "never completed: {open:?}");
    }

    fn start(id: &str, name: &str) -> EngineEvent {
        EngineEvent::ToolCallStart {
            id: id.into(),
            name: name.into(),
            args: json!({"command": "cargo test"}),
            is_sub_agent: false,
            preview: None,
        }
    }

    fn result(id: &str, outcome: ToolOutcome) -> EngineEvent {
        EngineEvent::ToolCallResult {
            id: id.into(),
            name: "Bash".into(),
            output: "Exit code: 0".into(),
            outcome,
            exit_code: Some(0),
            duration_ms: Some(12),
        }
    }

    fn text(t: &str) -> EngineEvent {
        EngineEvent::TextDelta { text: t.into() }
    }

    fn turn_end(reason: TurnEndReason) -> EngineEvent {
        EngineEvent::TurnEnd {
            turn_id: "t1".into(),
            reason,
        }
    }

    #[test]
    fn a_turn_becomes_items_that_start_and_complete() {
        let mut p = EventProjector::new("s1");
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (None, EngineEvent::ThinkingStart),
                (None, EngineEvent::ThinkingDelta { text: "hmm".into() }),
                (None, EngineEvent::ThinkingDone),
                (None, text("Let me ")),
                (None, text("check.")),
                (None, EngineEvent::TextDone),
                (None, start("c1", "Bash")),
                (
                    None,
                    EngineEvent::ToolOutputLine {
                        id: "c1".into(),
                        line: "ok".into(),
                        is_stderr: false,
                    },
                ),
                (None, result("c1", ToolOutcome::Success)),
                (None, text("Done.")),
                (None, EngineEvent::TextDone),
                (
                    None,
                    EngineEvent::Info {
                        message: "not on the protocol".into(),
                    },
                ),
                (
                    None,
                    EngineEvent::TurnSummary {
                        turn_id: "t1".into(),
                        usage: UsageTotals {
                            prompt_tokens: 100,
                            completion_tokens: 20,
                            api_calls: 2,
                            ..Default::default()
                        },
                        elapsed_ms: 1500,
                    },
                ),
                (None, turn_end(TurnEndReason::Complete)),
            ],
        );
        assert_eq!(
            methods(&lines),
            [
                "turn/started",
                "item/started reasoning",
                "item/reasoning/textDelta",
                "item/completed reasoning",
                "item/started agentMessage",
                "item/agentMessage/delta",
                "item/agentMessage/delta",
                "item/completed agentMessage",
                "item/started toolCall",
                "item/toolCall/outputDelta",
                "item/completed toolCall",
                "item/started agentMessage",
                "item/agentMessage/delta",
                "item/completed agentMessage",
                "turn/completed",
            ]
        );
        balanced(&lines);
        assert_eq!(lines[7]["params"]["item"]["text"], "Let me check.");
        assert_eq!(lines[9]["params"]["delta"], "ok\n");
        let call = &lines[10]["params"]["item"];
        assert_eq!(call["status"], "completed");
        assert_eq!(call["arguments"]["command"], "cargo test");
        assert_eq!(call["output"], "Exit code: 0");
        assert_eq!(call["kind"], "execute");
        assert_eq!(call["exitCode"], 0);
        assert_eq!(call["durationMs"], 12);
        let turn = &lines[14]["params"]["turn"];
        assert_eq!(turn["status"], "completed");
        assert_eq!(turn["usage"]["inputTokens"], 100);
        assert_eq!(turn["usage"]["modelCalls"], 2);
        assert_eq!(turn["durationMs"], 1500);
        assert!(lines.iter().all(|l| l["params"]["sessionId"] == "s1"));
    }

    /// An edit nobody was asked about carries its changes from its start
    /// to its end, as Codex's `fileChange` item does, and says it is an
    /// edit.
    #[test]
    fn an_edit_carries_its_changes_whether_or_not_anyone_was_asked() {
        use koda_core::preview::{DiffPreview, UnifiedDiffPreview};
        let mut p = EventProjector::new("s1");
        let preview = DiffPreview::UnifiedDiff(UnifiedDiffPreview {
            path: "notes.txt".into(),
            old_content: "one\ntwo\n".into(),
            new_content: "one\nthree\n".into(),
            hunks: Vec::new(),
            truncated: true,
        });
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (
                    None,
                    EngineEvent::ToolCallStart {
                        id: "e1".into(),
                        name: "Edit".into(),
                        args: json!({"file_path": "notes.txt"}),
                        is_sub_agent: false,
                        preview: Some(preview),
                    },
                ),
                (
                    None,
                    EngineEvent::ToolCallResult {
                        id: "e1".into(),
                        name: "Edit".into(),
                        output: "Edited notes.txt".into(),
                        outcome: ToolOutcome::Success,
                        exit_code: None,
                        duration_ms: None,
                    },
                ),
            ],
        );
        for line in &lines[1..3] {
            let call = &line["params"]["item"];
            assert_eq!(call["kind"], "edit", "{line}");
            let change = &call["changes"][0];
            assert_eq!(change["path"], "notes.txt");
            // The whole diff, not the preview's shortened hunks.
            let diff = change["diff"].as_str().unwrap();
            assert!(diff.contains("-two") && diff.contains("+three"), "{diff}");
        }
    }

    #[test]
    fn an_approval_is_a_request_resolved_by_id_and_a_refusal_keeps_its_detail() {
        let mut p = EventProjector::new("s1");
        let mut lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (None, start("c1", "Bash")),
                (
                    None,
                    EngineEvent::ApprovalRequest {
                        id: "engine-a".into(),
                        tool_call_id: "c1".into(),
                        tool_name: "Bash".into(),
                        detail: "rm -rf build".into(),
                        preview: None,
                        effect: koda_core::tools::ToolEffect::Destructive,
                        allow_for_session: false,
                    },
                ),
            ],
        );
        let resolved = p.resolved("engine-a").unwrap();
        lines.push(serde_json::from_str(&resolved.to_json().unwrap()).unwrap());
        assert!(p.resolved("engine-a").is_none(), "resolved once");
        lines.extend(run(
            &mut p,
            vec![(None, result("c1", ToolOutcome::Declined))],
        ));

        let request = &lines[2];
        assert_eq!(request["method"], "item/toolCall/requestApproval");
        assert_eq!(request["id"], 1);
        assert_eq!(request["params"]["itemId"], "c1");
        assert_eq!(request["params"]["effect"], "destructive");
        assert_eq!(lines[3]["method"], "serverRequest/resolved");
        assert_eq!(lines[3]["params"]["requestId"], 1);
        let call = &lines[4]["params"]["item"];
        assert_eq!(call["status"], "declined");
        assert_eq!(call["detail"], "rm -rf build");
    }

    #[test]
    fn a_sub_agent_s_items_name_their_parent_and_its_turn_is_not_one() {
        let mut p = EventProjector::new("s1");
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (None, start("agent", "InvokeAgent")),
                (
                    Some("agent"),
                    EngineEvent::TurnStart {
                        turn_id: "sub".into(),
                    },
                ),
                (Some("agent"), start("c1", "Read")),
                (Some("agent"), text("found it")),
                (Some("agent"), result("c1", ToolOutcome::Success)),
                (Some("agent"), turn_end(TurnEndReason::Complete)),
                (None, result("agent", ToolOutcome::Success)),
                (None, turn_end(TurnEndReason::Complete)),
            ],
        );
        balanced(&lines);
        let turns = lines
            .iter()
            .filter(|l| l["method"] == "turn/started")
            .count();
        assert_eq!(turns, 1);
        let read = lines
            .iter()
            .find(|l| l["params"]["item"]["id"] == "c1")
            .unwrap();
        assert_eq!(read["params"]["parentItemId"], "agent");
        let message = lines
            .iter()
            .find(|l| l["params"]["item"]["type"] == "agentMessage")
            .unwrap();
        assert_eq!(message["params"]["parentItemId"], "agent");
        assert!(lines[1]["params"].get("parentItemId").is_none());
    }

    #[test]
    fn what_is_open_when_a_turn_stops_completes_with_it() {
        let mut p = EventProjector::new("s1");
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (None, text("partial")),
                (None, start("c1", "Bash")),
                (None, turn_end(TurnEndReason::Cancelled)),
            ],
        );
        balanced(&lines);
        let call = lines
            .iter()
            .find(|l| l["method"] == "item/completed" && l["params"]["item"]["id"] == "c1")
            .unwrap();
        assert_eq!(call["params"]["item"]["status"], "cancelled");
        assert_eq!(
            lines.last().unwrap()["params"]["turn"]["status"],
            "interrupted"
        );
    }

    #[test]
    fn compaction_is_an_item_and_a_retry_an_error_that_will_retry() {
        let mut p = EventProjector::new("s1");
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (
                    None,
                    EngineEvent::Compaction {
                        trigger: CT::Threshold,
                        status: CS::Started {
                            context_percent: Some(91),
                        },
                        notice: None,
                    },
                ),
                (
                    None,
                    EngineEvent::Compaction {
                        trigger: CT::Threshold,
                        status: CS::Completed {
                            messages_removed: 12,
                            summary_tokens: Some(300),
                        },
                        notice: None,
                    },
                ),
                (
                    None,
                    EngineEvent::Compaction {
                        trigger: CT::Window,
                        status: CS::Completed {
                            messages_removed: 3,
                            summary_tokens: None,
                        },
                        notice: None,
                    },
                ),
                (
                    None,
                    EngineEvent::Retry {
                        reason: EngineRetryReason::RateLimit,
                        attempt: 1,
                        max_attempts: 5,
                        delay_ms: 1000,
                        notice: Some(Notice::Warn("Rate limited; retrying".into())),
                    },
                ),
                (
                    None,
                    turn_end(TurnEndReason::Error {
                        message: "gave up".into(),
                    }),
                ),
            ],
        );
        balanced(&lines);
        let done = &lines[2]["params"]["item"];
        assert_eq!(done["status"], "completed");
        assert_eq!(done["contextPercent"], 91);
        assert_eq!(done["summaryTokens"], 300);
        assert_eq!(lines[3]["method"], "item/started");
        assert_eq!(lines[4]["params"]["item"]["trigger"], "window");
        let error = &lines[5];
        assert_eq!(error["method"], "error");
        assert_eq!(error["params"]["willRetry"], true);
        assert_eq!(error["params"]["retry"]["reason"], "rateLimit");
        let turn = &lines[6]["params"]["turn"];
        assert_eq!(turn["status"], "failed");
        assert_eq!(turn["error"]["message"], "gave up");
    }

    /// A sub-agent's notice is its call's progress: sent at once, and kept
    /// on the completed item. The session's own `Info` lines aren't sent.
    #[test]
    fn a_notice_about_a_call_is_its_progress() {
        let mut p = EventProjector::new("s1");
        let info = |m: &str| EngineEvent::Info { message: m.into() };
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (None, start("call-1", "InvokeAgent")),
                (
                    Some("call-1"),
                    info("  \u{23f3} explore: reached max turns (3)"),
                ),
                (None, info("Running 2 tools in parallel...")),
                (None, result("call-1", ToolOutcome::Success)),
            ],
        );
        assert_eq!(
            methods(&lines),
            [
                "turn/started",
                "item/started toolCall",
                "item/toolCall/progress",
                "item/completed toolCall",
            ]
        );
        assert_eq!(lines[2]["params"]["itemId"], "call-1");
        assert_eq!(
            lines[2]["params"]["message"],
            "\u{23f3} explore: reached max turns (3)"
        );
        assert_eq!(
            lines[3]["params"]["item"]["progress"],
            json!(["\u{23f3} explore: reached max turns (3)"])
        );
    }

    /// A background agent's calls, sent after its turn has ended, count in
    /// the session's total and under the agent's name.
    #[test]
    fn a_background_agents_usage_counts_under_its_name_outside_a_turn() {
        let mut p = EventProjector::new("s1");
        let usage = UsageTotals {
            prompt_tokens: 70,
            completion_tokens: 7,
            ..Default::default()
        };
        let lines = run(
            &mut p,
            vec![
                (
                    Some("agent-a1"),
                    EngineEvent::SubAgentStart {
                        agent_name: "explore".into(),
                    },
                ),
                (Some("agent-a1"), EngineEvent::ModelUsage { usage }),
            ],
        );
        assert_eq!(methods(&lines), ["session/tokenUsage/updated"]);
        assert_eq!(p.tokens.total.input_tokens, 70);
        let explore = p.tokens.by_agent.get("explore").expect("under its name");
        assert_eq!(explore.input_tokens, 70);
        assert_eq!(explore.model_calls, 1);
        assert!(!p.tokens.by_agent.contains_key("main"));
    }

    /// Token counts go out after each call: the session's own calls as
    /// `last`, every call in `total`, a sub-agent's included.
    #[test]
    fn token_usage_goes_out_after_every_call() {
        let mut p = EventProjector::new("s1");
        let call = |prompt: i64, completion: i64| EngineEvent::ModelUsage {
            usage: UsageTotals {
                prompt_tokens: prompt,
                completion_tokens: completion,
                api_calls: 1,
                ..Default::default()
            },
        };
        let lines = run(
            &mut p,
            vec![
                (
                    None,
                    EngineEvent::TurnStart {
                        turn_id: "t1".into(),
                    },
                ),
                (
                    None,
                    EngineEvent::ContextUsage {
                        used: 90,
                        max: 1000,
                    },
                ),
                (None, call(100, 10)),
                (Some("call-1"), call(50, 5)),
                (
                    Some("call-1"),
                    EngineEvent::ContextUsage {
                        used: 40,
                        max: 1000,
                    },
                ),
                (
                    None,
                    EngineEvent::ContextUsage {
                        used: 120,
                        max: 1000,
                    },
                ),
            ],
        );
        assert_eq!(
            methods(&lines),
            [
                "turn/started",
                "session/tokenUsage/updated",
                "session/tokenUsage/updated"
            ]
        );
        assert_eq!(lines[1]["params"]["tokenUsage"]["last"]["totalTokens"], 110);
        let usage = &lines[2]["params"]["tokenUsage"];
        assert!(usage.get("contextTokens").is_none(), "{usage}");
        assert_eq!(usage["modelContextWindow"], 1000);
        assert_eq!(usage["last"]["inputTokens"], 100);
        assert_eq!(usage["last"]["totalTokens"], 110);
        assert_eq!(usage["total"]["inputTokens"], 150);
        assert_eq!(usage["total"]["outputTokens"], 15);
        assert_eq!(lines[2]["params"]["turnId"], "t1");
    }
}
