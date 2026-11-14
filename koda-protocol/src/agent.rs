//! The sub-agents a session's model can call (`InvokeAgent`): Koda's own
//! `agent/list`. Codex's sub-agents are threads it shows through items, and
//! its roles aren't listed over its protocol; Claude Code's `/agents`
//! manages definitions like these.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `agent/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentListParams {
    /// An open session; its project's agents are listed.
    pub session_id: String,
}

/// Where an agent's definition comes from. A project's overrides a
/// user's, which overrides a built-in one of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AgentSource {
    /// Koda's own.
    Builtin,
    /// The user's `<config>/agents/`.
    User,
    /// The project's `.koda/agents/`.
    Project,
}

/// One agent the model can call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// The name `InvokeAgent` takes.
    pub name: String,
    /// What it's for, in one line.
    pub description: String,
    /// Where it's defined.
    pub source: AgentSource,
}

/// The answer to `agent/list`, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentListResponse {
    /// The agents.
    pub data: Vec<AgentInfo>,
}
