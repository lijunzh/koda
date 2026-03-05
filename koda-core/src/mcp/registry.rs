//! MCP server registry — manages multiple MCP server connections.
//!
//! Handles startup, shutdown, tool routing, and server lifecycle.

use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::mcp::client::McpClient;
use crate::mcp::config::{self, McpServerConfig};
use crate::providers::ToolDefinition;

/// Separator between server name and tool name.
pub const TOOL_NAME_SEP: char = '.';

/// Registry of all connected MCP servers.
#[derive(Default)]
pub struct McpRegistry {
    /// Connected servers keyed by name.
    servers: HashMap<String, McpClient>,
}

impl McpRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// Load configs from `.mcp.json` files and connect to all servers.
    /// Logs errors for servers that fail to connect but doesn't abort.
    ///
    /// Returns a list of `(name, result)` status entries for the client to render.
    pub async fn start_from_config(
        &mut self,
        project_root: &Path,
    ) -> Vec<(String, Result<usize, String>)> {
        let configs = config::load_mcp_configs(project_root);
        if configs.is_empty() {
            return Vec::new();
        }

        let mut statuses = Vec::new();

        for (name, server_config) in configs {
            match McpClient::connect(name.clone(), server_config).await {
                Ok(client) => {
                    let tool_count = client.tool_definitions().len();
                    statuses.push((name.clone(), Ok(tool_count)));
                    self.servers.insert(name, client);
                }
                Err(e) => {
                    let msg = format_error_short(&e);
                    tracing::error!("Failed to connect MCP server '{name}': {e:#}");
                    statuses.push((name, Err(msg)));
                }
            }
        }

        statuses
    }

    /// Connect a single new server and add it to the registry.
    pub async fn add_server(&mut self, name: String, config: McpServerConfig) -> Result<()> {
        let client = McpClient::connect(name.clone(), config).await?;
        self.servers.insert(name, client);
        Ok(())
    }

    /// Remove and disconnect a server.
    pub fn remove_server(&mut self, name: &str) -> bool {
        self.servers.remove(name).is_some()
        // RunningService is dropped here, which closes the connection.
    }

    /// Restart a server (remove + reconnect from config).
    pub async fn restart_server(&mut self, name: &str, project_root: &Path) -> Result<()> {
        let configs = config::load_mcp_configs(project_root);
        let config = configs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("No config found for MCP server '{name}'"))?;
        self.remove_server(name);
        self.add_server(name.to_string(), config.clone()).await
    }

    /// Restart all servers from config.
    pub async fn restart_all(&mut self, project_root: &Path) {
        self.shutdown();
        self.start_from_config(project_root).await;
    }

    /// Get all tool definitions from all connected MCP servers.
    pub fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.servers
            .values()
            .flat_map(|client| client.tool_definitions())
            .cloned()
            .collect()
    }

    /// Execute an MCP tool call. The `namespaced_name` should be `server.tool_name`.
    /// Returns the tool output as a string.
    pub async fn call_tool(&self, namespaced_name: &str, arguments: &str) -> Result<String> {
        let (server_name, tool_name) =
            namespaced_name.split_once(TOOL_NAME_SEP).ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid MCP tool name '{namespaced_name}' — expected 'server.tool_name'"
                )
            })?;

        let client = self
            .servers
            .get(server_name)
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server_name}' is not connected"))?;

        client.call_tool(tool_name, arguments).await
    }

    /// Check if a tool name belongs to an MCP server.
    pub fn is_mcp_tool(&self, name: &str) -> bool {
        name.contains(TOOL_NAME_SEP)
            && name
                .split_once(TOOL_NAME_SEP)
                .is_some_and(|(server, _)| self.servers.contains_key(server))
    }

    /// Get info about all connected servers (for /mcp status display).
    pub fn server_info(&self) -> Vec<McpServerInfo> {
        self.servers
            .values()
            .map(|client| McpServerInfo {
                name: client.name.clone(),
                command: client.config.command.clone(),
                args: client.config.args.clone(),
                tool_count: client.tool_definitions().len(),
                tool_names: client
                    .tool_definitions()
                    .iter()
                    .map(|t| t.name.clone())
                    .collect(),
            })
            .collect()
    }

    /// Gracefully shut down all servers.
    pub fn shutdown(&mut self) {
        let count = self.servers.len();
        if count > 0 {
            tracing::info!("Shutting down {count} MCP server(s)");
        }
        self.servers.clear(); // Drop all RunningService handles
    }
}

/// Display info for a connected MCP server.
#[derive(Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub tool_count: usize,
    pub tool_names: Vec<String>,
}

/// Format an error to a single short line (no backtrace noise).
fn format_error_short(err: &anyhow::Error) -> String {
    let msg = format!("{err}");
    // Take just the first line
    msg.lines().next().unwrap_or(&msg).to_string()
}
