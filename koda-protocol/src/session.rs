//! Sessions: one conversation with its model, mode and tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::TrustMode;

/// A session as it starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// The session's id, for resuming it.
    pub id: String,
    /// The model id, such as `claude-sonnet-5-5`.
    pub model: String,
    /// The provider, such as `lm-studio` or `anthropic`.
    pub provider: String,
    /// The context window in tokens.
    pub context_window: u64,
    /// What Koda may do without asking.
    pub mode: TrustMode,
    /// The tools the model is offered.
    pub tools: Vec<String>,
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
