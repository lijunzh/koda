//! Items: the parts of a turn. Each starts and completes, with deltas in
//! between for those that stream.
//!
//! Koda has one item for every tool call, named by its tool, where Codex has
//! one per kind (`commandExecution`, `fileChange`, `mcpToolCall`…): Koda's
//! tools share one shape, and typed fields such as a command or a diff are
//! added when a client needs them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A part of a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    /// What the user said: a turn's prompt, or steering text added while it
    /// ran, as the model received it. Steering texts that reached the model
    /// together are one item.
    #[serde(rename_all = "camelCase")]
    UserMessage {
        /// The item's id.
        id: String,
        /// The text.
        text: String,
        /// The client's ids for the messages it carries, where it gave them.
        client_message_ids: Vec<String>,
    },
    /// Text from the model. Complete text on `item/completed`.
    #[serde(rename_all = "camelCase")]
    AgentMessage {
        /// The item's id.
        id: String,
        /// The text so far: empty when it starts.
        text: String,
    },
    /// The model's reasoning, where the provider shows it.
    #[serde(rename_all = "camelCase")]
    Reasoning {
        /// The item's id.
        id: String,
        /// The text so far: empty when it starts.
        text: String,
    },
    /// A tool call.
    #[serde(rename_all = "camelCase")]
    ToolCall {
        /// The call's id, from the model.
        id: String,
        /// The tool, such as `Bash` or `Edit`.
        tool: String,
        /// The arguments the model gave.
        arguments: Value,
        /// Where it stands.
        status: ToolCallStatus,
        /// What the model reads: the tool's output, or why it didn't run.
        /// Set on completion.
        output: Option<String>,
        /// What the call would do, in a line, when Koda asked about it or
        /// blocked it.
        detail: Option<String>,
    },
    /// Context compaction: old history summarized or dropped to make room.
    #[serde(rename_all = "camelCase")]
    ContextCompaction {
        /// The item's id.
        id: String,
        /// What set it off.
        trigger: CompactionTrigger,
        /// Where it stands.
        status: CompactionStatus,
        /// Context used when it began, in percent, where it was measured.
        context_percent: Option<u64>,
        /// Rows taken out of the context, when completed.
        messages_removed: Option<u64>,
        /// Estimated size of the summary, when there is one.
        summary_tokens: Option<u64>,
        /// Why it was skipped or failed.
        reason: Option<String>,
    },
}

impl Item {
    /// The item's id.
    pub fn id(&self) -> &str {
        match self {
            Self::UserMessage { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::ContextCompaction { id, .. } => id,
        }
    }
}

/// Where a tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ToolCallStatus {
    /// Waiting or running.
    InProgress,
    /// It ran and did what was asked.
    Completed,
    /// It ran and failed: bad arguments, an error, a timeout, or a command
    /// that exited non-zero.
    Failed,
    /// It never ran: refused at an approval, by plan mode or by a
    /// sub-agent's limits.
    Declined,
    /// The turn stopped before or while it ran.
    Cancelled,
}

/// What set off a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CompactionTrigger {
    /// Context passed the auto-compact threshold before a model call.
    Threshold,
    /// The provider rejected a request as too long.
    Overflow,
    /// The estimate was past the window, so history was dropped unsummarized.
    Window,
    /// The client asked for it with `session/compact/start`.
    Manual,
    /// The turn's final reply left the context past the after-turn
    /// threshold; it ran before `turn/completed`.
    PostTurn,
}

/// Where a compaction stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CompactionStatus {
    /// Summarizing.
    InProgress,
    /// History was replaced.
    Completed,
    /// A precondition stopped it; nothing changed.
    Skipped,
    /// The summary call failed; nothing changed.
    Failed,
}

/// `item/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// On a sub-agent's items: the tool call that started the sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    /// The item as it starts.
    pub item: Item,
}

/// `item/completed`. Every item that starts completes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// On a sub-agent's items: the tool call that started the sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    /// The item, complete.
    pub item: Item,
}

/// `item/agentMessage/delta` and `item/reasoning/textDelta`: more text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextDeltaNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The item it belongs to.
    pub item_id: String,
    /// The new text.
    pub delta: String,
}

/// `item/toolCall/outputDelta`: a line of a running command's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallOutputDeltaNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The tool call.
    pub item_id: String,
    /// The output, ending in a newline.
    pub delta: String,
    /// Which stream it came from.
    pub stream: OutputStream,
}

/// A command's output stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// `item/toolCall/requestApproval`: a tool call needs a yes before it runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallApprovalParams {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The tool call.
    pub item_id: String,
    /// The tool.
    pub tool: String,
    /// What the call would do, in a line.
    pub detail: String,
    /// Why it needs a yes.
    pub effect: ToolEffect,
    /// Whether `approveForSession` may be answered: only for local changes
    /// inside the project.
    pub allow_for_session: bool,
}

/// The answer to `item/toolCall/requestApproval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallApprovalResponse {
    /// Yes or no.
    pub decision: ApprovalDecision,
}

/// Yes or no to a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ApprovalDecision {
    /// Run it.
    Approve,
    /// Run it, and the same command or file again this session.
    ApproveForSession,
    /// Don't run it.
    Reject,
    /// Don't run it, and tell the model why.
    #[serde(rename_all = "camelCase")]
    RejectWithFeedback {
        /// What the model is told.
        feedback: String,
    },
}

/// What a tool call does, and so whether it needs a yes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ToolEffect {
    /// Reads only.
    ReadOnly,
    /// Acts on a remote service.
    RemoteAction,
    /// Changes local files or state.
    LocalMutation,
    /// Irreversible or far-reaching: `rm -rf`, `git push --force`.
    Destructive,
}

/// `item/tool/requestUserInput`: the model asked the user a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserInputParams {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The `AskUser` call.
    pub item_id: String,
    /// The question.
    pub question: String,
    /// Answer choices; empty for a free answer.
    pub options: Vec<String>,
}

/// The answer to `item/tool/requestUserInput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserInputResponse {
    /// The answer; empty for none.
    pub answer: String,
}
