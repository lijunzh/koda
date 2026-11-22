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
    /// The size, in bytes of compact JSON, past which a tool's input schema
    /// is shrunk, as Codex's `tool_input_schema_max_bytes` (default 5,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input_schema_max_bytes: Option<usize>,
    /// Where the server's tools are left out, as Codex's `omit_tools_from`:
    /// `["deferred"]` sends them up front even to a model with tool search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omit_tools_from: Option<Vec<String>>,
    /// The OAuth scopes a sign-in asks for, as Codex's `scopes`; unset, the
    /// scopes the server advertises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
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
    /// The resources it offers, as Codex lists them: what `@server:uri`
    /// names in a message. MCP Apps pages (`ui://`) are left out.
    #[serde(default)]
    pub resources: Vec<McpResource>,
    /// How Koda authenticates to it, as Codex's `authStatus`. Given in the
    /// configured listing (no `sessionId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_status: Option<McpAuthStatus>,
}

/// How Koda authenticates to a server; Codex's values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpAuthStatus {
    /// An HTTP server with neither a bearer token nor a sign-in: it may need
    /// none, or answer 401.
    Unknown,
    /// A stdio server: no authentication.
    Unsupported,
    /// A bearer token from an environment variable.
    BearerToken,
    /// Signed in with OAuth (`mcpServer/oauth/login`).
    #[serde(rename = "oAuth")]
    OAuth,
}

/// `mcpServer/oauth/login`: sign in to a configured streamable HTTP server
/// with OAuth, as Codex's. The answer gives the page to open; the host waits
/// for the browser's redirect, stores the tokens and sends
/// `mcpServer/oauthLogin/completed`. Open sessions then reconnect to their
/// servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerOauthLoginParams {
    /// The server's name in the config.
    pub name: String,
    /// The OAuth scopes to ask for. Unset or empty, the server's configured
    /// `scopes`, or else the scopes it advertises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

/// The answer to `mcpServer/oauth/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerOauthLoginResponse {
    /// The authorization server's page, to open in a browser.
    pub authorization_url: String,
}

/// `mcpServer/oauth/logout`: forget a server's stored OAuth tokens. Koda's
/// own method; Codex forgets them in `codex mcp logout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerOauthLogoutParams {
    /// The server's name.
    pub name: String,
}

/// The answer to `mcpServer/oauth/logout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerOauthLogoutResponse {
    /// Whether there were tokens to forget.
    pub removed: bool,
}

/// `mcpServer/oauthLogin/completed`: a sign-in started with
/// `mcpServer/oauth/login` finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerOauthLoginCompletedNotification {
    /// The server.
    pub name: String,
    /// Whether the tokens are stored.
    pub success: bool,
    /// Why not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One resource a server offers, MCP's `Resource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpResource {
    /// Its URI.
    pub uri: String,
    /// Its name.
    pub name: String,
    /// A title for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What it is, as the server says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Its MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
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

/// `mcpServer/elicitation/request`: an MCP server asks the user for input
/// in the middle of a tool call, as MCP's `elicitation/create` and Codex's
/// request of the same name. Form mode asks for the fields in
/// `requestedSchema`; URL mode asks the user to visit `url`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestParams {
    /// The session.
    pub session_id: String,
    /// The server asking.
    pub server_name: String,
    /// What the server says it needs.
    pub message: String,
    /// Form mode: a JSON Schema object of flat, primitive fields (string,
    /// number, integer, boolean, string enums).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_schema: Option<serde_json::Value>,
    /// URL mode: the page to visit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The answer to `mcpServer/elicitation/request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestResponse {
    /// What the user did.
    pub action: McpServerElicitationAction,
    /// The fields' values, for an accepted form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
}

/// What the user did with an elicitation: MCP's actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum McpServerElicitationAction {
    /// Gave what was asked.
    Accept,
    /// Said no; the tool goes on without it.
    Decline,
    /// Dismissed it; the operation stops.
    Cancel,
}
