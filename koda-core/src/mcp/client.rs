//! MCP client wrapper around the `rmcp` crate.
//!
//! Provides a minimal client that can connect to an MCP server via stdio,
//! list available tools, and call tools.

use anyhow::{Context, Result};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation, ProtocolVersion,
        Tool as McpTool,
    },
    service::RunningService,
    transport::TokioChildProcess,
};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::mcp::config::McpServerConfig;
use crate::providers::ToolDefinition;
use crate::tools::ToolEffect;

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
        let mut info = ClientInfo::default();
        info.protocol_version = ProtocolVersion::V_2025_03_26;
        info.capabilities = ClientCapabilities::builder().build();
        info.client_info = Implementation::new("koda", env!("CARGO_PKG_VERSION"));
        info
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
    /// Effect classification per namespaced tool name (from MCP annotations).
    tool_effects: std::collections::HashMap<String, ToolEffect>,
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
            .list_tools(Default::default())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to list tools from '{name}': {e}"))?;

        // Convert to Koda ToolDefinition format with namespacing
        let tools = tools_result
            .tools
            .iter()
            .map(|t| mcp_tool_to_definition(&name, t))
            .collect();

        // Extract ToolEffect classification from MCP annotations
        let tool_effects = tools_result
            .tools
            .iter()
            .map(|t| {
                let namespaced = format!("{name}.{}", t.name);
                let effect = classify_mcp_annotations(t);
                (namespaced, effect)
            })
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
            tool_effects,
            _timeout: timeout,
        })
    }

    /// Get the namespaced tool definitions for this server.
    pub fn tool_definitions(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Get the ToolEffect for a namespaced tool name.
    pub fn tool_effect(&self, namespaced_name: &str) -> Option<ToolEffect> {
        self.tool_effects.get(namespaced_name).copied()
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

        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(args) = args {
            params = params.with_arguments(args);
        }

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

/// Classify an MCP tool's effect from its annotations.
///
/// MCP spec annotations (all optional hints):
/// - `readOnlyHint: true`  → ReadOnly
/// - `destructiveHint: true` → Destructive
/// - Neither → LocalMutation (conservative default)
fn classify_mcp_annotations(tool: &McpTool) -> ToolEffect {
    match &tool.annotations {
        Some(ann) => {
            if ann.read_only_hint == Some(true) {
                ToolEffect::ReadOnly
            } else if ann.destructive_hint == Some(true) {
                ToolEffect::Destructive
            } else {
                // No hints or readOnly=false → assume local mutation
                ToolEffect::LocalMutation
            }
        }
        None => ToolEffect::LocalMutation,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ToolAnnotations;
    use std::sync::Arc;

    fn empty_schema() -> Arc<serde_json::Map<String, serde_json::Value>> {
        let mut map = serde_json::Map::new();
        map.insert("type".to_string(), serde_json::json!("object"));
        Arc::new(map)
    }

    fn make_tool(name: &str, annotations: Option<ToolAnnotations>) -> McpTool {
        let mut tool = McpTool::new(name.to_string(), "", empty_schema());
        tool.annotations = annotations;
        tool
    }

    #[test]
    fn test_classify_read_only_hint() {
        let tool = make_tool(
            "list_items",
            Some(ToolAnnotations::default().read_only(true)),
        );
        assert_eq!(classify_mcp_annotations(&tool), ToolEffect::ReadOnly);
    }

    #[test]
    fn test_classify_destructive_hint() {
        let tool = make_tool(
            "drop_table",
            Some(ToolAnnotations::default().destructive(true)),
        );
        assert_eq!(classify_mcp_annotations(&tool), ToolEffect::Destructive);
    }

    #[test]
    fn test_classify_no_annotations() {
        let tool = make_tool("unknown", None);
        assert_eq!(classify_mcp_annotations(&tool), ToolEffect::LocalMutation);
    }

    #[test]
    fn test_classify_read_only_false() {
        let tool = make_tool(
            "write_file",
            Some(ToolAnnotations::default().read_only(false)),
        );
        assert_eq!(classify_mcp_annotations(&tool), ToolEffect::LocalMutation);
    }

    #[test]
    fn test_classify_destructive_trumps_when_not_readonly() {
        let tool = make_tool("nuke", Some(ToolAnnotations::default().destructive(true)));
        assert_eq!(classify_mcp_annotations(&tool), ToolEffect::Destructive);
    }
}
