//! Koda's notes between client and host: `session/memory/read` and
//! `session/memory/add`, scoped to a session as Codex scopes
//! `thread/memory/read`, since the project notes are the session's
//! project's. Codex's memories come only from its background pipeline;
//! Koda's are added by the model (`MemoryWrite`) or by hand, as Claude
//! Code's are. The instruction files a session reads are on
//! [`crate::session::Session::instruction_sources`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Which of Koda's notes: the session's project's or everyone's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum MemoryScope {
    /// The session's project's notes.
    Project,
    /// The notes every project reads.
    Global,
}

/// `session/memory/read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionMemoryReadParams {
    /// An open session.
    pub session_id: String,
}

/// One notes file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemoryNote {
    /// Whose notes.
    pub scope: MemoryScope,
    /// Where the host keeps them.
    pub path: String,
    /// What they say; absent until something is saved there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The answer to `session/memory/read`: the project's notes, then the
/// global ones, each as the model reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionMemoryReadResponse {
    /// The project's notes, then the global ones.
    pub notes: Vec<MemoryNote>,
}

/// `session/memory/add`: save `text` to the notes. Text that starts with a
/// `## Heading` replaces the section with that heading, if there is one,
/// as `MemoryWrite` does. The session's next turn reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionMemoryAddParams {
    /// An open session.
    pub session_id: String,
    /// Whose notes.
    pub scope: MemoryScope,
    /// What to save; not blank.
    pub text: String,
}

/// The answer to `session/memory/add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionMemoryAddResponse {
    /// The file it went to.
    pub path: String,
}
