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
