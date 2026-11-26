//! Items: the parts of a turn. Each starts and completes, with deltas in
//! between for those that stream.
//!
//! Koda has one item for every tool call, named by its tool, where Codex has
//! one per kind (`commandExecution`, `fileChange`, `mcpToolCall`…): Koda's
//! tools share one shape, and typed fields such as a command or a diff are
//! added when a client needs them, as `changes` was for a file edit's diff.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool call's `output` when Plan mode refused it, with status
/// `declined`: what the model reads in place of the tool's result. It says
/// why and what to do instead, and nothing about switching modes: a
/// headless run can't, and in a session a person answers, leaving Plan is
/// theirs to do.
/// Clients read its `[plan mode]` start as "refused by the mode".
pub const PLAN_BLOCKED_MESSAGE: &str = "[plan mode] Not run: this session is read-only, so \
     only tools that read are allowed. Don't retry it or try another way to make the change; \
     say in your answer what you would do instead.";

/// Opens a `Bash` approval's `detail` when the call asks to run outside
/// the sandbox, so a client can tell it from an ordinary destructive
/// command.
pub const OUTSIDE_SANDBOX_TAG: &str = "[outside the sandbox]";

/// A part of a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    /// What the user said: a turn's prompt, or steering text added while it
    /// ran. Steering texts that reached the model together are one item.
    #[serde(rename_all = "camelCase")]
    UserMessage {
        /// The item's id.
        id: String,
        /// What the user sent, as `turn/start` or `turn/steer` took it, in
        /// order, as Codex's `userMessage` keeps its `content`. Images read
        /// from mentioned files aren't here: the mention names them.
        content: Vec<crate::turn::UserInput>,
        /// The client's ids for the messages it carries, where it gave them
        /// (`clientUserMessageId` on `turn/start` and `turn/steer`). Codex's
        /// item carries one, `clientId`; Koda's can carry several, since
        /// steering texts that reach the model together are one item.
        #[serde(alias = "clientMessageIds")]
        client_user_message_ids: Vec<String>,
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
        /// What it does, as Koda classifies the call, the effect an approval
        /// names, so a client can colour or group it. Absent in turns stored
        /// before Koda sent it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<ToolEffect>,
        /// What sort of tool it is, from ACP's list, so a client can pick
        /// a card or an icon without knowing every tool's name. Absent in
        /// turns stored before Koda sent it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ToolKind>,
        /// Where it stands.
        status: ToolCallStatus,
        /// What the model reads: the tool's output, or why it didn't run.
        /// Set on completion.
        output: Option<String>,
        /// The exit code of the command a `Bash` or `Cargo` call ran, on
        /// completion, as Codex's `commandExecution` item has its
        /// `exitCode`. Absent for other tools and for a command that never
        /// exited (it timed out, was stopped or went to the background).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// How long the tool ran, in milliseconds, on completion, not
        /// counting a wait for approval, as Codex's items have
        /// `durationMs`. Absent for a call that never ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// What the call would do, in a line, when Koda asked about it or
        /// blocked it.
        detail: Option<String>,
        /// The file changes an `Edit` or `Write` makes, from its start,
        /// whether or not anyone was asked about it: Codex's `fileChange`
        /// item's `changes`, carried on Koda's one tool-call item.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<FileUpdateChange>,
        /// The notices sent about it as `item/toolCall/progress` while it
        /// ran, in order: a sub-agent's, for an `InvokeAgent` call. Set on
        /// completion. A skill's change of tool scope is sent once its call
        /// has completed, so it isn't here.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        progress: Vec<String>,
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

/// A change to one file, in Codex's `FileUpdateChange` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    /// The file, as the tool call names it.
    pub path: String,
    /// Added, deleted or changed.
    pub kind: PatchChangeKind,
    /// For a change, the unified diff; for a new file, its content.
    pub diff: String,
}

/// What happens to a file, as Codex's `PatchChangeKind` says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PatchChangeKind {
    /// A new file.
    Add,
    /// A file removed.
    Delete,
    /// A file changed, and moved when `move_path` is set. Codex's spelling,
    /// in snake case as Codex's is.
    Update {
        /// Where it moves to. Koda's tools never move a file.
        #[serde(alias = "movePath")]
        move_path: Option<String>,
    },
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
    /// When it started, in Unix milliseconds. Codex's `startedAtMs`.
    pub started_at_ms: i64,
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
    /// When it completed, in Unix milliseconds. Codex's `completedAtMs`.
    pub completed_at_ms: i64,
}

/// An item of a stored turn, as `session/start` (resuming) and
/// `session/read` return it: what `item/completed` sent, with its parent
/// and its times, as Codex's `ThreadItem` keeps them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnItem {
    /// On a sub-agent's items: the tool call that started the sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_item_id: Option<String>,
    /// The item.
    pub item: Item,
    /// When it started, in Unix milliseconds; absent when not recorded, as
    /// in sessions stored before Koda recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    /// When it completed, in Unix milliseconds; absent when not recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
}

/// `item/toolCall/progress`: a notice about a running tool call, such as a
/// sub-agent reaching its turn limit. Codex's `item/mcpToolCall/progress`,
/// for Koda's one tool-call item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallProgressNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The tool call.
    pub item_id: String,
    /// The notice, in a line.
    pub message: String,
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
    /// The file changes it would make, as on its item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<FileUpdateChange>,
    /// The command a `Bash` or `Cargo` call would run, as Codex's command
    /// approval's `command`. `detail` says the same in a line; this is for
    /// a client that lays the dialog out itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The folder the command runs in, as Codex's `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Why the model wants the command run, when it said, as Codex's
    /// `reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether the command asks to run outside the sandbox, with the
    /// user's yes for this one call. Koda's own: Codex asks for more than
    /// the sandbox gives with `additionalPermissions`. `detail` then starts
    /// with [`OUTSIDE_SANDBOX_TAG`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside_sandbox: bool,
    /// Whether the command runs in the background.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// Why it needs a yes.
    pub effect: ToolEffect,
    /// Whether `approveForSession` may be answered: only for local changes
    /// inside the project.
    pub allow_for_session: bool,
    /// The sub-agent asking, when it isn't the main agent. Koda's own: Codex
    /// heads a sub-agent's request with its thread, and Claude Code names the
    /// sub-agent. A background agent may ask while no turn runs; `turnId` is
    /// then empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<ApprovalAgent>,
}

/// The sub-agent behind an approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAgent {
    /// Which agent it is (`explore`, `task`, ...).
    pub name: String,
    /// A background agent's id; absent for one a turn is waiting on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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

/// What sort of tool a call is: the Agent Client Protocol's `ToolKind`,
/// whose values ACP clients such as Zed draw tool calls by. Codex has an
/// item type per kind instead; Koda keeps one tool-call item and says its
/// kind, which maps straight onto ACP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads files or data.
    Read,
    /// Changes files or content.
    Edit,
    /// Removes files or data.
    Delete,
    /// Moves or renames files.
    Move,
    /// Searches for information.
    Search,
    /// Runs a command or code.
    Execute,
    /// Thinks or plans: a to-do list, a question to the user.
    Think,
    /// Fetches from the network.
    Fetch,
    /// Changes the session's mode.
    SwitchMode,
    /// Anything else: sub-agents, MCP tools, skills.
    Other,
}

impl ToolKind {
    /// The kind of Koda's tool `name`, after the mapping Qwen Code gives
    /// its own tools onto ACP's kinds. A tool Koda doesn't know (an MCP
    /// server's) is `Other`, as ACP's MCP bridges send it.
    pub fn of(name: &str) -> Self {
        match name {
            "Read" | "List" | "ListAgents" | "ListSkills" | "ActivateSkill" | "MemoryRead" => {
                Self::Read
            }
            "Edit" | "Write" | "MemoryWrite" => Self::Edit,
            "Grep" | "Glob" | "RecallContext" | "WebSearch" => Self::Search,
            "Bash" | "Cargo" | "WriteStdin" | "Monitor" => Self::Execute,
            "TodoWrite" | "AskUser" => Self::Think,
            "WebFetch" => Self::Fetch,
            _ => Self::Other,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_kind_is_named_as_acp_names_it() {
        assert_eq!(ToolKind::of("Bash"), ToolKind::Execute);
        assert_eq!(ToolKind::of("Edit"), ToolKind::Edit);
        assert_eq!(ToolKind::of("WebFetch"), ToolKind::Fetch);
        assert_eq!(ToolKind::of("mcp__mail__send"), ToolKind::Other);
        assert_eq!(
            serde_json::to_value(ToolKind::SwitchMode).unwrap(),
            "switch_mode"
        );
        assert_eq!(serde_json::to_value(ToolKind::Execute).unwrap(), "execute");
    }

    /// Turns stored before the names followed Codex's still read, and are
    /// sent with Codex's names. (A `userMessage` stored before prompts were
    /// lists is read by the server's history; see `history::stored_item`.)
    #[test]
    fn turns_stored_under_the_old_names_still_read() {
        let moved: PatchChangeKind =
            serde_json::from_value(serde_json::json!({"type": "update", "movePath": "b.rs"}))
                .unwrap();
        assert_eq!(
            serde_json::to_value(&moved).unwrap(),
            serde_json::json!({"type": "update", "move_path": "b.rs"})
        );

        let turn: crate::turn::Turn = serde_json::from_value(serde_json::json!({
            "id": "t1", "status": "completed", "error": null, "durationMs": 5,
            "usage": {"inputTokens": 10, "cachedInputTokens": 0, "cacheCreationTokens": 3,
                      "outputTokens": 2, "reasoningTokens": 1, "modelCalls": 1},
        }))
        .unwrap();
        let usage = turn.usage.unwrap();
        assert_eq!(usage.cache_write_input_tokens, 3);
        assert_eq!(usage.reasoning_output_tokens, 1);
    }
}
