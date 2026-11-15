//! Feedback: `feedback/save`, Codex's, which writes a report and, with the
//! user's consent, the session's logs and history to a ZIP on the host.
//! Codex can also upload one (`feedback/upload`); Koda has nowhere to send
//! it, so a person attaches the ZIP to an issue.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `feedback/save`: Codex's fields, less `extraLogFiles` and `tags`, which
/// nothing in Koda sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackSaveParams {
    /// What kind of report: `bug`, `bad_result`, `good_result` or `other`,
    /// Codex's names.
    pub classification: String,
    /// What happened, in the user's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The session it's about, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Whether the ZIP holds the logs and session history, which can quote
    /// prompts, commands and tool output. Without it, only the report.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_logs: bool,
    /// Where to write the ZIP on the host: an absolute path that doesn't
    /// exist yet, as Codex requires.
    pub save_path: String,
}

/// The answer to `feedback/save`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSaveResponse {
    /// Where the ZIP was written.
    pub saved_path: String,
    /// The session it's about, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}
