//! A session's background processes, the commands the model started with
//! `Bash {background: true}`, which outlive the turn. Codex's
//! `thread/backgroundTerminals/*`, under Koda's noun: `list`, `terminate`
//! and `clean`. Stopping a turn leaves them running, as in Codex; a client
//! stops them with these, and Koda stops any left when it exits. When one
//! exits by itself, Koda tells the model, as Claude Code does, and reports
//! it with `session/backgroundTerminals/exited`.

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
    /// Whether the session will start a turn by itself for the model's
    /// background commands: one is still running, or has ended and the
    /// model hasn't been told yet. A sub-agent's commands don't count, nor
    /// do processes an ordinary command left running.
    /// Koda's own; `koda exec` waits while it's true, as `claude -p` waits
    /// for background work.
    #[serde(default)]
    pub waiting: bool,
    /// The model's background agents still running, oldest first. Koda's
    /// own; Claude Code lists its background agents beside its background
    /// commands. `terminate` stops one by its `agentId`, and `clean` stops
    /// them with the processes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<BackgroundAgent>,
}

/// A background agent that is still running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundAgent {
    /// Its id, as the model has it.
    pub agent_id: String,
    /// Which agent it is (`explore`, `task`, ...).
    pub agent_name: String,
    /// The task it was last given.
    pub prompt: String,
    /// When its latest run started, in Unix milliseconds.
    pub started_at: i64,
}

/// `session/backgroundTerminals/terminate`: stop one, and whatever it
/// started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTerminalsTerminateParams {
    /// An open session.
    pub session_id: String,
    /// From the list: a process's `processId`, or an agent's `agentId`.
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

/// `session/backgroundTerminals/exited`: background processes the model
/// started have exited by themselves, and the model is being told, as
/// Claude Code tells Claude when a background task finishes. Its
/// `turnId` is the turn that tells it: a running one, or one Koda starts
/// for it when the session is idle. A process a client or Koda stopped
/// isn't reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalsExitedNotification {
    /// The session.
    pub session_id: String,
    /// The turn that tells the model.
    pub turn_id: String,
    /// The processes.
    pub data: Vec<BackgroundTerminalExit>,
}

/// A background process that exited by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTerminalExit {
    /// Its id, as `list` gave it.
    pub process_id: String,
    /// The command the model ran.
    pub command: String,
    /// Its exit code; absent when a signal ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}
