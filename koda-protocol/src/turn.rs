//! Turns: one request from the user and everything Koda does for it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A turn. `turn/started` sends it in progress, `turn/completed` finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    /// The turn's id.
    pub id: String,
    /// Where it stands.
    pub status: TurnStatus,
    /// Why it failed, when it did; and on an interrupted turn, why it
    /// didn't stop cleanly, when it was dropped where it stood.
    pub error: Option<TurnError>,
    /// What it used: tokens and model calls, sub-agents and compaction
    /// included. Set when the turn is over.
    pub usage: Option<Usage>,
    /// Wall-clock time, set when the turn is over.
    pub duration_ms: Option<u64>,
    /// When it started, in Unix seconds, as Codex's `Turn.startedAt`. Not
    /// on the `turn/start` response, whose `turn/started` carries it, nor
    /// on turns of sessions stored before 0.8.0.
    #[serde(default)]
    pub started_at: Option<i64>,
    /// When it ended, in Unix seconds, as Codex's `Turn.completedAt`. Not
    /// on a turn that ended with the process running it.
    #[serde(default)]
    pub completed_at: Option<i64>,
    /// Its items, in order. Filled only when a stored session's turns are
    /// returned, by `session/start` resuming it, `session/read` or
    /// `session/turns/list`, as Codex's `Turn.items` is; empty on `turn/started` and
    /// `turn/completed`, whose items come as notifications.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<crate::item::TurnItem>,
}

/// Where a turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// Running.
    InProgress,
    /// The model finished.
    Completed,
    /// The user or client stopped it.
    Interrupted,
    /// It failed, after any retries.
    Failed,
}

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    /// One sentence for a person.
    pub message: String,
    /// What kind of failure, for a client to act on, when Koda knows one,
    /// as Codex's turn errors carry `codexErrorInfo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<TurnErrorCode>,
}

/// What kind of failure a [`TurnError`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnErrorCode {
    /// The turn spent its token budget (`turn_token_budget`) and stopped
    /// before its next model call. What it did so far stays in the session.
    TokenBudgetSpent,
}

/// Tokens and model calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens read from the provider's cache.
    pub cached_input_tokens: u64,
    /// Input tokens written to the provider's cache. Codex's
    /// `cacheWriteInputTokens`.
    #[serde(alias = "cacheCreationTokens")]
    pub cache_write_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Output tokens spent reasoning. Codex's `reasoningOutputTokens`.
    #[serde(alias = "reasoningTokens")]
    pub reasoning_output_tokens: u64,
    /// Model calls that finished.
    pub model_calls: u64,
    /// Web searches run on the provider's servers (a Claude model's
    /// `WebSearch`), which it bills apart from tokens, as Claude Code
    /// reports `webSearchRequests`. Koda's own.
    #[serde(default)]
    pub web_search_requests: u64,
}

/// `turn/start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnStartParams {
    /// The session.
    pub session_id: String,
    /// What the user sent, in the order they composed it: text, images,
    /// mentioned files and picked skills, as Codex's `input`. A `$name` in
    /// a text part that names a skill counts as picking it.
    pub input: Vec<UserInput>,
    /// The client's id for the message, echoed on its `userMessage` item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    /// The trust mode from this turn on. Without one, the session keeps
    /// its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::state::TrustMode>,
    /// A JSON Schema the last reply of this turn, and of the turns Koda
    /// starts after it on its own (a goal's, a background command's), must
    /// match, as Codex's `outputSchema`. The prompt says so; a reply that
    /// doesn't match gets one more model call with the schema enforced by
    /// the API. A `turn/start` without one clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

/// One part of what the user sends, Codex's `UserInput`, with its names
/// (`text_elements` in snake case, as Codex spells it). A prompt is a list
/// of these in the order the user composed it, as Codex's, the Agent Client
/// Protocol's and Claude's are.
///
/// Koda keeps the order on the `userMessage` item. The model is sent one
/// message as before the list: the text parts in order with each mention's
/// contents and each skill's instructions after them, and the images, which
/// go before the text for Claude (as Anthropic recommends) and after it for
/// an OpenAI-compatible model. Images aren't placed between text parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// Text the user typed.
    Text {
        /// The text.
        text: String,
        /// Spans of `text` the client drew as something else (a pasted
        /// image's placeholder, say), kept on the item for the client to
        /// draw again; not sent to the model. Codex's.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        text_elements: Vec<TextElement>,
    },
    /// An image, as a `data:` URL (`data:image/png;base64,...`). Codex's
    /// `image` also takes an `https` URL or an uploaded file's id; Koda
    /// takes data URLs only.
    Image {
        /// The image as a `data:` URL.
        url: String,
    },
    /// An image file on the host's machine, read under the Read tool's
    /// rules. Codex's `localImage`.
    LocalImage {
        /// Its path, relative to the session's folder or absolute.
        path: String,
    },
    /// A skill the user picked, as `skills/list` gives it: the model is
    /// sent its instructions. Codex's.
    Skill {
        /// The skill's name, which decides.
        name: String,
        /// Its `SKILL.md`.
        path: String,
    },
    /// A file, folder or MCP resource the user mentioned with `@`: the
    /// model is sent a file's contents, a folder's listing, or a resource
    /// as `@server:uri` names it, as Claude Code includes an `@` file
    /// (unless `file_mentions` is off). Codex's `mention` part, with
    /// Koda's meaning: Codex's names an app, plugin or thread, and Codex
    /// leaves a file as a path in the text for the model to read.
    Mention {
        /// What the client shows for it, such as the file's name.
        name: String,
        /// Its path, relative to the session's folder or absolute, or
        /// `server:uri` for an MCP resource.
        path: String,
    },
}

/// A span of a text part that the client drew as something else. Codex's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TextElement {
    /// The span, in bytes of the text.
    pub byte_range: ByteRange,
    /// What the client shows in its place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

/// A range of bytes, end exclusive. Codex's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ByteRange {
    /// The first byte.
    pub start: usize,
    /// The byte after the last.
    pub end: usize,
}

impl UserInput {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }

    /// A mention of `path`, shown by its last part.
    pub fn mention(path: impl Into<String>) -> Self {
        let path = path.into();
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&path)
            .to_string();
        Self::Mention { name, path }
    }

    /// An image given as `media_type` and base64 `data`.
    pub fn image(media_type: &str, data: &str) -> Self {
        Self::Image {
            url: format!("data:{media_type};base64,{data}"),
        }
    }
}

/// The text of `input`: its text parts, in order, a blank line between.
pub fn text_of(input: &[UserInput]) -> String {
    input
        .iter()
        .filter_map(|part| match part {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A `data:` URL's media type and base64 content, if it is one.
pub fn data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    (!media_type.is_empty()).then_some((media_type, data))
}

/// The answer to `turn/start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    /// The turn, in progress.
    pub turn: Turn,
}

/// `turn/interrupt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnInterruptParams {
    /// The session.
    pub session_id: String,
    /// The turn to stop. Refused when it isn't the one running.
    pub turn_id: String,
}

/// The answer to `turn/interrupt`, sent once the turn has stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnInterruptResponse {}

/// `turn/steer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TurnSteerParams {
    /// The session.
    pub session_id: String,
    /// The turn the text is for. Refused when it isn't the one running.
    pub expected_turn_id: String,
    /// What the user said, as Codex's `input`. Only text parts for now:
    /// steering reaches the model as text, so images, mentions and skills
    /// go with `turn/start`.
    pub input: Vec<UserInput>,
    /// The client's id for the text, echoed on the `userMessage` item that
    /// delivers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
}

/// The answer to `turn/steer`: the text is queued for the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    /// The turn it was queued for.
    pub turn_id: String,
}

/// `turn/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    /// The session.
    pub session_id: String,
    /// The turn, in progress.
    pub turn: Turn,
}

/// `turn/completed`: the turn is over, however it ended. Exactly one per
/// turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    /// The session.
    pub session_id: String,
    /// The turn, finished.
    pub turn: Turn,
}

/// `turn/diff/updated`: what the turn's edits have changed so far, as one
/// unified diff (git's format, paths relative to the session's folder), as
/// Codex's. Sent after each edit that changes it and again when the turn
/// completes. Each file is compared with its content when the turn first
/// edited it, so a file edited twice shows once; changes made by commands
/// (`Bash`) and by sub-agents aren't in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnDiffUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The whole diff so far. Empty when the turn's edits have been undone,
    /// and from an edit Koda couldn't track on: the turn then has no diff.
    pub diff: String,
}

/// `turn/plan/updated`: the model rewrote its plan (its todo list).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The whole plan, in order.
    pub plan: Vec<PlanStep>,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    /// What the step is.
    pub step: String,
    /// How far it got.
    pub status: PlanStepStatus,
}

/// How far a plan step got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// `turn/loopGuard`: the model kept repeating itself. Koda's own; Codex has
/// none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnLoopGuardNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// What Koda did about it.
    pub action: LoopGuardAction,
    /// What was repeated.
    pub detail: String,
}

/// What the loop guard did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LoopGuardAction {
    /// Told the model to change course; the turn goes on.
    Feedback,
    /// The model didn't, so the turn stops.
    Stop,
}

/// `turn/loopCap/requestDecision`: the turn reached its iteration cap.
/// Until it is answered the turn waits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopCapParams {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The cap.
    pub cap: u32,
    /// The tools called most recently.
    pub recent_tools: Vec<String>,
}

/// The answer to `turn/loopCap/requestDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopCapResponse {
    /// Go on or stop.
    pub decision: LoopCapDecision,
}

/// Whether a turn at its cap goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LoopCapDecision {
    /// 50 more iterations.
    Continue50,
    /// 200 more iterations.
    Continue200,
    /// Stop the turn.
    Stop,
}
