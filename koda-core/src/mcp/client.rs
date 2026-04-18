//! Single MCP server client — wraps rmcp connection lifecycle.
//!
//! Each `McpClient` owns one connection to one MCP server.
//! It handles spawning (stdio) or connecting (HTTP), initialization,
//! tool discovery, and tool invocation.

use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::child_process::TokioChildProcess;
use serde_json::Value;
use tokio::process::Command;

use super::config::{McpServerConfig, McpTransport};
use super::tool_bridge::McpToolAnnotations;
use crate::providers::ToolDefinition;
use crate::tools::web_fetch::is_safe_url;

/// Connection status of a single MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpClientStatus {
    /// Not yet connected.
    Disconnected,
    /// Connection in progress.
    Connecting,
    /// Connected and ready.
    Connected,
    /// Connection failed.
    Failed,
}

/// A discovered MCP tool with its Koda-side definition and annotations.
#[derive(Debug, Clone)]
pub struct DiscoveredTool {
    /// Koda tool definition (qualified name, description, schema).
    pub definition: ToolDefinition,
    /// MCP annotations for trust classification.
    pub annotations: McpToolAnnotations,
    /// Original (unqualified) tool name on the MCP server.
    pub original_name: String,
}

/// Client for a single MCP server.
pub struct McpClient {
    /// Server name (user-assigned, e.g. "playwright").
    name: String,
    /// Server configuration.
    config: McpServerConfig,
    /// Running rmcp service (None when disconnected).
    service: Option<RunningService<rmcp::service::RoleClient, ()>>,
    /// Discovered tools after connection.
    tools: Vec<DiscoveredTool>,
    /// Current connection status.
    status: McpClientStatus,
    /// Error message from the last failed connection attempt.
    last_error: Option<String>,
    /// Optional human-readable instructions returned by the server during
    /// `initialize`. Injected into the system prompt so the model picks up
    /// per-server guidance (#922). `None` when the server provides none.
    instructions: Option<String>,
}

impl McpClient {
    /// Create a new (disconnected) client for the given server.
    pub fn new(name: String, config: McpServerConfig) -> Self {
        Self {
            name,
            config,
            service: None,
            tools: Vec::new(),
            status: McpClientStatus::Disconnected,
            last_error: None,
            instructions: None,
        }
    }

    /// Server name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current connection status.
    pub fn status(&self) -> McpClientStatus {
        self.status
    }

    /// Last error message (if status is Failed).
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Discovered tools (empty until connected).
    pub fn tools(&self) -> &[DiscoveredTool] {
        &self.tools
    }

    /// Server-provided instructions from the `initialize` response, if any.
    /// Returns `None` if disconnected or the server didn't provide instructions.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    /// Connect to the MCP server, initialize, and discover tools.
    ///
    /// Dispatches to stdio or HTTP transport based on config.
    pub async fn connect(&mut self) -> Result<()> {
        self.status = McpClientStatus::Connecting;
        self.last_error = None;

        let timeout = Duration::from_secs(self.config.startup_timeout_sec);

        match tokio::time::timeout(timeout, self.connect_inner()).await {
            Ok(Ok(())) => {
                let transport_label = match &self.config.transport {
                    McpTransport::Stdio { .. } => "stdio",
                    McpTransport::Http { .. } => "http",
                };
                self.status = McpClientStatus::Connected;
                tracing::info!(
                    server = %self.name,
                    transport = transport_label,
                    tools = self.tools.len(),
                    "MCP server connected"
                );
                Ok(())
            }
            Ok(Err(e)) => {
                self.status = McpClientStatus::Failed;
                self.last_error = Some(e.to_string());
                tracing::warn!(
                    server = %self.name,
                    error = %e,
                    "MCP server connection failed"
                );
                Err(e)
            }
            Err(_) => {
                self.status = McpClientStatus::Failed;
                let msg = format!(
                    "MCP server '{}' startup timed out after {}s",
                    self.name, self.config.startup_timeout_sec
                );
                self.last_error = Some(msg.clone());
                tracing::warn!(server = %self.name, "{msg}");
                Err(anyhow::anyhow!(msg))
            }
        }
    }

    /// Inner connect logic — dispatches to the right transport.
    async fn connect_inner(&mut self) -> Result<()> {
        // Clone transport to avoid borrowing self.config while calling &mut self methods.
        let transport = self.config.transport.clone();
        match transport {
            McpTransport::Stdio {
                ref command,
                ref args,
                ref env,
                ref cwd,
            } => self.connect_stdio(command, args, env, cwd.as_deref()).await,
            McpTransport::Http {
                ref url,
                ref bearer_token,
                ref headers,
            } => {
                self.connect_http(url, bearer_token.as_deref(), headers)
                    .await
            }
        }
    }

    /// Connect via stdio transport (spawn child process).
    async fn connect_stdio(
        &mut self,
        command: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<()> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        for (key, val) in env {
            cmd.env(key, val);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }

        let transport =
            TokioChildProcess::new(cmd).context("failed to spawn MCP server process")?;
        let service = ().serve(transport).await.context("MCP handshake failed")?;
        self.finish_handshake(service).await
    }

    /// Connect via Streamable HTTP transport.
    async fn connect_http(
        &mut self,
        url: &str,
        bearer_token: Option<&str>,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        use http::{HeaderName, HeaderValue};
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

        // SSRF protection: reject private/internal URLs before opening any connection.
        if !is_safe_url(url) {
            anyhow::bail!(
                "MCP HTTP URL '{url}' is not allowed: private, loopback, or link-local \
                 addresses are blocked to prevent SSRF attacks"
            );
        }

        // Warn if sending a bearer token over plaintext HTTP.
        if bearer_token.is_some() && url.starts_with("http://") {
            tracing::warn!(
                server = %self.name,
                url = %url,
                "MCP bearer token is being sent over plaintext HTTP — use HTTPS in production"
            );
        }

        let mut config = StreamableHttpClientTransportConfig::with_uri(url);

        // Set bearer token.  rmcp's StreamableHttpClientTransport passes
        // auth_header to reqwest's `bearer_auth()`, which prepends "Bearer "
        // automatically — so we store the raw token, not "Bearer {token}".
        if let Some(token) = bearer_token {
            config.auth_header = Some(token.to_string());
        }

        // Set custom headers.
        if !headers.is_empty() {
            let mut header_map = std::collections::HashMap::new();
            for (k, v) in headers {
                let name = HeaderName::try_from(k.as_str())
                    .with_context(|| format!("invalid HTTP header name: {k}"))?;
                let value = HeaderValue::try_from(v.as_str())
                    .with_context(|| format!("invalid HTTP header value for {k}"))?;
                header_map.insert(name, value);
            }
            config.custom_headers = header_map;
        }

        // Enable session recovery for remote servers.
        config.reinit_on_expired_session = true;

        let transport = StreamableHttpClientTransport::from_config(config);
        let service = ().serve(transport).await.context("MCP HTTP handshake failed")?;
        self.finish_handshake(service).await
    }

    /// Common post-handshake bookkeeping shared by stdio + HTTP connect paths.
    /// Captures the server's `instructions` (#922) and discovers tools.
    async fn finish_handshake(
        &mut self,
        service: RunningService<rmcp::service::RoleClient, ()>,
    ) -> Result<()> {
        // Capture server-provided instructions before storing the service so
        // we don't have to re-borrow it. Filter empties to keep prompt clean.
        self.instructions = service
            .peer()
            .peer_info()
            .and_then(|info| info.instructions.clone())
            .filter(|s| !s.trim().is_empty());
        self.service = Some(service);
        self.discover_tools().await
    }

    /// Fetch the tool list from the connected server.
    async fn discover_tools(&mut self) -> Result<()> {
        let service = self.service.as_ref().context("not connected")?;

        let result = service
            .list_tools(Default::default())
            .await
            .context("failed to list MCP tools")?;

        self.tools.clear();

        for tool in result.tools {
            let tool_name: &str = &tool.name;

            // Apply tool filtering.
            if !self.config.is_tool_allowed(tool_name) {
                tracing::debug!(
                    server = %self.name,
                    tool = %tool_name,
                    "MCP tool filtered out by config"
                );
                continue;
            }

            let (definition, annotations) =
                super::tool_bridge::mcp_tool_to_definition(&self.name, &tool);

            self.tools.push(DiscoveredTool {
                definition,
                annotations,
                original_name: tool_name.to_string(),
            });
        }

        Ok(())
    }

    /// Call a tool on this MCP server.
    ///
    /// `tool_name` is the original (unqualified) name on the server.
    /// `arguments` is the JSON arguments value.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<rmcp::model::CallToolResult> {
        let service = self.service.as_ref().context("MCP server not connected")?;

        let timeout = Duration::from_secs(self.config.tool_timeout_sec);

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Value::Object(map) = arguments {
            params.arguments = Some(map);
        }

        let result = tokio::time::timeout(timeout, service.call_tool(params))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MCP tool call '{}' on server '{}' timed out after {}s",
                    tool_name,
                    self.name,
                    self.config.tool_timeout_sec
                )
            })?
            .context("MCP tool call failed")?;

        Ok(result)
    }

    /// Disconnect from the MCP server.
    pub async fn disconnect(&mut self) {
        if let Some(service) = self.service.take() {
            // RunningService is dropped, which cleans up the child process.
            drop(service);
        }
        self.tools.clear();
        self.status = McpClientStatus::Disconnected;
        self.last_error = None;
        tracing::info!(server = %self.name, "MCP server disconnected");
    }
}

impl McpClient {
    /// Force status to a specific value (test-only).
    #[cfg(feature = "test-support")]
    pub fn set_status_for_test(&mut self, status: McpClientStatus) {
        self.status = status;
    }

    /// Force server instructions (test-only). Pass `None` to clear.
    #[cfg(feature = "test-support")]
    pub fn set_instructions_for_test(&mut self, instructions: Option<String>) {
        self.instructions = instructions;
    }

    /// Force last error (test-only).
    #[cfg(feature = "test-support")]
    pub fn set_last_error_for_test(&mut self, err: Option<String>) {
        self.last_error = err;
    }

    /// Inject discovered tools directly (test-only).
    ///
    /// Lets manager-level tests exercise success-path code (annotation
    /// caching, `all_tool_definitions`, status summaries) without
    /// spawning a real MCP server subprocess.
    #[cfg(feature = "test-support")]
    pub fn set_tools_for_test(&mut self, tools: Vec<DiscoveredTool>) {
        self.tools = tools;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Ensure the service is dropped (child process cleaned up).
        if self.service.is_some() {
            tracing::debug!(server = %self.name, "McpClient dropped while still connected");
        }
    }
}
