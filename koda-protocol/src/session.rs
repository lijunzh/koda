//! Sessions: one conversation with its model, mode and tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::TrustMode;

/// A session: one stored conversation. Codex's `Thread`. What it's running
/// with (`model` through `tools`) is there only while it is open, as from
/// `session/start`; `session/list` gives stored sessions without it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The session's id, for resuming it.
    pub id: String,
    /// Its title, set from the first prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The first user message; empty before there is one.
    #[serde(default)]
    pub preview: String,
    /// When it was created, in Unix seconds.
    pub created_at: i64,
    /// When its last message was stored, in Unix seconds.
    pub updated_at: i64,
    /// The project it works in; absent for sessions too old to record it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The model id, such as `claude-sonnet-5-5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The provider, such as `lm-studio` or `anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// What Koda may do without asking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<TrustMode>,
    /// The tools the model is offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

/// `session/start`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionStartParams {
    /// An earlier session to resume, by its full id. Without one, a new
    /// session starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Who answers the session's requests. Without one, the client does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
}

/// Who answers a session's approvals, questions and loop-cap decisions.
/// Codex's `approvalPolicy` names the same choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalPolicy {
    /// The client answers each request.
    #[default]
    OnRequest,
    /// Nobody is there to ask, as in `koda exec`: Koda approves what the
    /// mode allows, refuses destructive actions and anything outside the
    /// sandbox, answers a question with nothing and goes on at the loop
    /// cap. Each request is still sent, followed at once by
    /// `serverRequest/resolved`, so a reader sees what was decided.
    Never,
}

/// The answer to `session/start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartResponse {
    /// The session, ready for a turn.
    pub session: Session,
}

/// `session/started`: a session is ready for its first turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartedNotification {
    /// The session.
    pub session: Session,
    /// The Koda release, such as `0.7.0`.
    pub koda_version: String,
    /// [`crate::PROTOCOL_VERSION`]. A reader that can't read it should stop.
    pub protocol_version: u32,
}

/// `session/unsubscribe`: the client is done with a session. Its running
/// turn, if any, stops, and the session is closed; `session/start` with its
/// id opens it again. Codex's `thread/unsubscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUnsubscribeParams {
    /// The session.
    pub session_id: String,
}

/// The answer to `session/unsubscribe`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionUnsubscribeResponse {
    /// What happened.
    pub status: SessionUnsubscribeStatus,
}

/// What `session/unsubscribe` did. Codex also has `notSubscribed`, for a
/// session other clients still hold; a Koda host has one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionUnsubscribeStatus {
    /// The session was open, and now isn't.
    Unsubscribed,
    /// No session with that id was open.
    NotLoaded,
}

/// `session/list`: stored sessions, newest first. Codex's `thread/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionListParams {
    /// The project whose sessions to list; without one, the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How many at most; without one, 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// The answer to `session/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionListResponse {
    /// The sessions, without what open ones are running with.
    pub data: Vec<Session>,
}

/// `session/delete`: delete a stored session. An open one is closed first,
/// its running turn stopped, as Codex's `thread/delete` does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// The session, by its full id.
    pub session_id: String,
}

/// The answer to `session/delete`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeleteResponse {}

/// `session/compact/start`: compact an open session's history now, as
/// Codex's `thread/compact/start`. Answered at once; the compaction runs as
/// a turn of its own (`turn/started`, a `contextCompaction` item with the
/// `manual` trigger, `turn/completed`). A running turn is stopped first and
/// ends interrupted. The compaction's turn can be interrupted, not steered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCompactStartParams {
    /// The session.
    pub session_id: String,
}

/// The answer to `session/compact/start`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactStartResponse {}

/// `session/deleted`: a session was deleted. Codex's `thread/deleted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionDeletedNotification {
    /// The session.
    pub session_id: String,
}
