//! A session's background processes, the commands the model started with
//! `Bash {background: true}`, which outlive the turn. Codex's
//! `thread/backgroundTerminals/*`, under Koda's noun: `list`, `terminate`
//! and `clean`. Stopping a turn leaves them running, as in Codex; a client
//! stops them with these, and Koda stops any left when it exits.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `session/backgroundTerminals/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTerminalsListParams {
    /// An open session.
    pub session_id: String,
}

/// A background process that is still running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminal {
    /// Its id, for `terminate`: the process group's id, as a string.
    pub process_id: String,
    /// The command the model ran.
    pub command: String,
    /// The folder it started in.
    pub cwd: String,
    /// The operating system's id for the process leading its group.
    pub os_pid: u32,
    /// When it started, in Unix milliseconds. Koda's own; Codex has none.
    pub started_at: i64,
}

/// The answer to `session/backgroundTerminals/list`: the running ones,
/// oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalsListResponse {
    /// The running processes.
    pub data: Vec<BackgroundTerminal>,
}

/// `session/backgroundTerminals/terminate`: stop one, and whatever it
/// started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTerminalsTerminateParams {
    /// An open session.
    pub session_id: String,
    /// From the list.
    pub process_id: String,
}

/// The answer to `session/backgroundTerminals/terminate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalsTerminateResponse {
    /// Whether it was running and is now being stopped.
    pub terminated: bool,
}

/// `session/backgroundTerminals/clean`: stop them all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTerminalsCleanParams {
    /// An open session.
    pub session_id: String,
}

/// The answer to `session/backgroundTerminals/clean`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalsCleanResponse {
    /// How many were running and are now being stopped. Koda's own; Codex
    /// answers with nothing.
    pub terminated: u32,
}
