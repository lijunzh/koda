//! MCP client wrapper around the `rmcp` crate.
//!
//! Provides a minimal client that can connect to an MCP server via stdio,
//! list available tools, and call tools.

use anyhow::{Context, Result};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation,
        PaginatedRequestParams, ProtocolVersion, Tool as McpTool,
    },
    service::RunningService,
    transport::TokioChildProcess,
};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::mcp::config::McpServerConfig;
use crate::providers::ToolDefinition;

/// Default timeout for tool calls (seconds).
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 30;

/// Minimal MCP client handler. We don't need sampling or fancy notification
/// handling — just log what comes in.
#[derive(Debug, Clone)]
struct KodaClientHandler;

impl ClientHandler for KodaClientHandler {
    // All trait methods have defaults — we accept them.
    // Notifications (progress, logging) are silently handled by rmcp defaults.

    fn get_info(&self) -> ClientInfo {
        ClientInfo {
            meta: None,
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ClientCapabilities::builder().build(),
            client_info: Implementation {
                name: "koda".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                title: None,
                description: None,
                website_url: None,
            },
        }
    }
}

/// A connected MCP server with cached tool definitions.
pub struct McpClient {
    /// The server name (from config key).
    pub name: String,
    /// Original config used to start this server.
    pub config: McpServerConfig,
    /// The running rmcp service (Peer methods available via Deref).
    service: RunningService<RoleClient, KodaClientHandler>,
    /// Cached tool definitions (converted to Koda format).
    tools: Vec<ToolDefinition>,
    /// Timeout for tool calls.
    _timeout: Duration,
}

impl McpClient {
    /// Connect to an MCP server by spawning its process.
    pub async fn connect(name: String, config: McpServerConfig) -> Result<Self> {
        let timeout = Duration::from_secs(config.timeout.unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS));

        // Build the subprocess command
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Spawn via rmcp's TokioChildProcess transport
        let (transport, _stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| {
                format!(
                    "Failed to spawn MCP server '{name}': {} {}",
                    config.command,
                    config.args.join(" ")
                )
            })?;

        // Connect and perform the MCP handshake
        let handler = KodaClientHandler;
        let service = handler
            .serve(transport)
            .await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed for '{name}': {e}"))?;

        // Discover available tools via the Peer high-level API
        let tools_result = service
            .list_tools(Some(PaginatedRequestParams {
                meta: None,
                cursor: None,
            }))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list tools from '{name}': {e}"))?;

        // Convert to Koda ToolDefinition format with namespacing
        let tools = tools_result
            .tools
            .iter()
            .map(|t| mcp_tool_to_definition(&name, t))
            .collect();

        tracing::info!(
            "MCP server '{}' connected — {} tools available",
            name,
            tools_result.tools.len()
        );

        Ok(Self {
            name,
            config,
            service,
            tools,
            _timeout: timeout,
        })
    }

    /// Get the namespaced tool definitions for this server.
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Call a tool on this MCP server.
    /// `tool_name` should be the *original* (un-namespaced) MCP tool name.
    pub async fn call_tool(&self, tool_name: &str, arguments: &str) -> Result<String> {
        let args: Option<serde_json::Map<String, serde_json::Value>> =
            if arguments.is_empty() || arguments == "{}" {
                None
            } else {
                Some(serde_json::from_str(arguments).with_context(|| {
                    format!("Invalid JSON arguments for MCP tool '{tool_name}'")
                })?)
            };

        let params = CallToolRequestParams {
            meta: None,
            task: None,
            name: tool_name.to_string().into(),
            arguments: args,
        };

        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| anyhow::anyhow!("MCP tool '{}' call failed: {e}", tool_name))?;

        Ok(format_call_result(&result))
    }
}

/// Convert an MCP Tool to a Koda ToolDefinition with namespaced name.
fn mcp_tool_to_definition(server_name: &str, tool: &McpTool) -> ToolDefinition {
    let namespaced_name = format!("{server_name}.{}", tool.name);
    let description = tool
        .description
        .as_deref()
        .unwrap_or("No description")
        .to_string();

    // The MCP tool's input_schema is already a JSON Schema object
    let parameters = serde_json::to_value(&tool.input_schema).unwrap_or_default();

    ToolDefinition {
        name: namespaced_name,
        description,
        parameters,
    }
}

/// Format a CallToolResult into a human-readable string.
fn format_call_result(result: &rmcp::model::CallToolResult) -> String {
    let mut output = String::new();
    for content in &result.content {
        if let Some(text) = content.as_text() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&text.text);
        }
    }
    if result.is_error.unwrap_or(false) && output.is_empty() {
        output = "MCP tool returned an error with no details.".to_string();
    }
    output
}
