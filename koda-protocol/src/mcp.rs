//! MCP servers between client and host, in Codex's shapes:
//! `mcpServerStatus/list`, `config/mcpServer/reload` and the
//! `mcpServer/startupStatus/updated` notification. Servers are config: a
//! client adds one with `config/batchWrite` at `mcp_servers.<name>` and
//! removes it with `null` there, as Codex's `config.toml` keeps
//! `[mcp_servers.<name>]`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The config key that holds MCP servers: `mcp_servers.<name>`.
pub const SERVERS_KEY: &str = "mcp_servers";

/// One MCP server's config, in Codex's `[mcp_servers.<name>]` shape: a
/// `command` for a server Koda starts, or a `url` for one it reaches over
/// streamable HTTP.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfigValue {
    /// The command that starts the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Its arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment variables set for it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The directory it starts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The server's URL, for streamable HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The environment variable that holds the bearer token. The token
    /// itself is never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    /// Headers sent with every HTTP request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub http_headers: BTreeMap<String, String>,
    /// Seconds the server has to start (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    /// Seconds a tool call has (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    /// A session doesn't start without it: `session/start` fails when it
    /// fails to start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// Only these tools are offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tools: Option<Vec<String>>,
    /// These tools are never offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_tools: Option<Vec<String>>,
}

/// `mcpServerStatus/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListMcpServerStatusParams {
    /// The session whose servers to list. Without one, the configured servers,
    /// none of them started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Only this server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}

/// The answer to `mcpServerStatus/list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListMcpServerStatusResponse {
    /// The servers, by name.
    pub data: Vec<McpServerStatus>,
}

/// One server as a session has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    /// The server's name.
    pub name: String,
    /// Where its connection stands.
    pub runtime_status: McpServerConnectionStatus,
    /// Its tools, by name.
    pub tools: BTreeMap<String, McpTool>,
    /// Why it has no tools, when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_error: Option<String>,
}

/// One of a server's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    /// Its name on the server.
    pub name: String,
    /// What it does, as the server says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a server's connection stands; Codex's values that Koda produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpServerConnectionStatus {
    /// Not started: stored, but no session has it.
    NotStarted,
    /// Starting.
    Starting,
    /// Connected; its tools are offered.
    Connected,
    /// It failed to start.
    Failed,
}

/// `config/mcpServer/reload`: every open session takes the configured servers
/// again, starting new ones, stopping removed ones and restarting changed
/// or failed ones, before its next turn. Answered once the idle sessions
/// have taken them, so a `mcpServerStatus/list` that follows shows the
/// change; new servers are then still starting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerRefreshParams {}

/// The answer to `config/mcpServer/reload`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerRefreshResponse {}

/// `mcpServer/startupStatus/updated`: a session's server started, failed
/// or was stopped while starting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatusUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The server.
    pub name: String,
    /// Where its startup stands.
    pub status: McpServerStartupState,
    /// Why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where a server's startup stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpServerStartupState {
    /// Starting.
    Starting,
    /// Started; its tools are offered.
    Ready,
    /// It failed to start.
    Failed,
    /// Stopped before it had started.
    Cancelled,
}
