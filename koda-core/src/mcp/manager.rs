//! Multi-server connection manager — owns all MCP clients.
//!
//! `McpManager` is the single entry point for the rest of koda-core.
//! It loads configs from the DB, connects to servers in parallel,
//! discovers tools, and routes tool calls to the right server.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;

use super::client::{McpClient, McpClientStatus};
use super::config::{self, McpServerConfig};
use super::tool_bridge::{McpToolAnnotations, parse_mcp_tool_name};
use crate::db::Database;
use crate::providers::ToolDefinition;
use crate::tools::ToolEffect;

/// Manager for all MCP server connections.
///
/// Owns the set of `McpClient` instances and provides a unified interface
/// for tool discovery and execution.
pub struct McpManager {
    /// Connected (or attempted) clients, keyed by server name.
    clients: HashMap<String, McpClient>,

    /// Cached tool annotations for classify_tool lookups.
    /// Keyed by qualified tool name (`server__tool`).
    annotations: HashMap<String, McpToolAnnotations>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    /// Create an empty manager (no servers configured).
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            annotations: HashMap::new(),
        }
    }

    /// Load configs from the database and connect to all servers in parallel.
    ///
    /// Errors from individual servers are logged but don't fail the whole
    /// startup — the manager connects what it can and reports status.
    pub async fn start_from_db(db: &Database) -> Result<Self> {
        let configs = config::load_mcp_configs(db).await?;

        if configs.is_empty() {
            tracing::debug!("no MCP servers configured");
            return Ok(Self::new());
        }

        tracing::info!(
            count = configs.len(),
            servers = ?configs.keys().collect::<Vec<_>>(),
            "starting MCP servers"
        );

        let mut manager = Self::new();
        manager.connect_all(configs).await;
        Ok(manager)
    }

    /// Connect to all servers in parallel.
    async fn connect_all(&mut self, configs: HashMap<String, McpServerConfig>) {
        // Spawn connect tasks in parallel.
        let handles: Vec<_> = configs
            .into_iter()
            .map(|(name, config)| {
                tokio::spawn(async move {
                    let mut client = McpClient::new(name.clone(), config);
                    let result = client.connect().await;
                    (name, client, result)
                })
            })
            .collect();

        // Collect results.
        for handle in handles {
            match handle.await {
                Ok((name, client, result)) => {
                    if let Err(e) = &result {
                        tracing::warn!(
                            server = %name,
                            error = %e,
                            "MCP server failed to connect (non-fatal)"
                        );
                    }
                    // Register tools from successful connections.
                    for tool in client.tools() {
                        self.annotations
                            .insert(tool.definition.name.clone(), tool.annotations.clone());
                    }
                    self.clients.insert(name, client);
                }
                Err(e) => {
                    tracing::error!(error = %e, "MCP server connect task panicked");
                }
            }
        }

        let connected = self
            .clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Connected)
            .count();
        let total = self.clients.len();
        tracing::info!(connected, total, "MCP server startup complete");
    }

    /// Get all discovered tool definitions across all connected servers.
    pub fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Connected)
            .flat_map(|c| c.tools().iter().map(|t| t.definition.clone()))
            .collect()
    }

    /// Classify an MCP tool's effect using cached annotations.
    pub fn classify_tool(&self, qualified_name: &str) -> ToolEffect {
        let annotations = self.annotations.get(qualified_name);
        super::tool_bridge::classify_mcp_tool(annotations)
    }

    /// Call a tool by its qualified name (`server__tool`).
    ///
    /// Parses the name to find the right server, then delegates.
    pub async fn call_tool(&self, qualified_name: &str, arguments: Value) -> Result<String> {
        let (server_name, tool_name) = parse_mcp_tool_name(qualified_name)
            .context("invalid MCP tool name format (expected server__tool)")?;

        let client = self
            .clients
            .get(server_name)
            .context(format!("MCP server '{server_name}' not found"))?;

        if client.status() != McpClientStatus::Connected {
            anyhow::bail!(
                "MCP server '{server_name}' is not connected (status: {:?})",
                client.status()
            );
        }

        let result = client.call_tool(tool_name, arguments).await?;

        // Convert CallToolResult content to a string.
        let output = call_tool_result_to_string(&result);

        // Check for error flag.
        if result.is_error.unwrap_or(false) {
            anyhow::bail!("MCP tool error: {output}");
        }

        Ok(output)
    }

    /// Check whether a qualified name belongs to a registered MCP tool.
    pub fn has_tool(&self, qualified_name: &str) -> bool {
        self.annotations.contains_key(qualified_name)
    }

    /// Get a summary of all servers and their status.
    pub fn status_summary(&self) -> Vec<McpServerStatus> {
        self.clients
            .values()
            .map(|c| McpServerStatus {
                name: c.name().to_string(),
                status: c.status(),
                tool_count: c.tools().len(),
                error: c.last_error().map(String::from),
            })
            .collect()
    }

    /// Disconnect all servers.
    pub async fn shutdown(&mut self) {
        for client in self.clients.values_mut() {
            client.disconnect().await;
        }
        self.annotations.clear();
        tracing::info!("all MCP servers disconnected");
    }

    /// Add a server at runtime (hot-reload).
    ///
    /// Connects immediately. If a server with this name already exists,
    /// it is disconnected first.
    pub async fn add_server(&mut self, name: String, config: McpServerConfig) -> Result<()> {
        // Disconnect any existing server with this name.
        if let Some(mut old) = self.clients.remove(&name) {
            old.disconnect().await;
            // Remove stale annotations.
            self.annotations.retain(|k, _| {
                parse_mcp_tool_name(k)
                    .map(|(s, _)| s != name)
                    .unwrap_or(true)
            });
        }

        let mut client = McpClient::new(name.clone(), config);
        client.connect().await?;

        // Cache annotations for the new tools.
        for tool in client.tools() {
            self.annotations
                .insert(tool.definition.name.clone(), tool.annotations.clone());
        }

        self.clients.insert(name, client);
        Ok(())
    }

    /// Remove and disconnect a server by name.
    ///
    /// Returns `true` if a server was found and removed.
    pub async fn remove_server(&mut self, name: &str) -> bool {
        if let Some(mut client) = self.clients.remove(name) {
            client.disconnect().await;
            self.annotations.retain(|k, _| {
                parse_mcp_tool_name(k)
                    .map(|(s, _)| s != name)
                    .unwrap_or(true)
            });
            tracing::info!(server = %name, "MCP server removed");
            true
        } else {
            false
        }
    }

    /// Is the manager empty (no servers configured)?
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Number of connected servers.
    pub fn connected_count(&self) -> usize {
        self.clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Connected)
            .count()
    }
}

/// Status summary for a single MCP server.
#[derive(Debug, Clone)]
pub struct McpServerStatus {
    /// Server name.
    pub name: String,
    /// Connection status.
    pub status: McpClientStatus,
    /// Number of discovered tools.
    pub tool_count: usize,
    /// Last error message (if failed).
    pub error: Option<String>,
}

/// Convert MCP CallToolResult content into a plain string.
///
/// Text content is concatenated. Non-text content (images, blobs) is
/// described inline so the LLM knows something was returned.
fn call_tool_result_to_string(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();

    for content in &result.content {
        match &content.raw {
            rmcp::model::RawContent::Text(text) => {
                parts.push(text.text.clone());
            }
            other => {
                // Describe non-text content so the LLM knows it was returned.
                let kind = format!("{:?}", std::mem::discriminant(other));
                tracing::debug!(content_type = %kind, "MCP tool returned non-text content");
                parts.push(format!("[non-text content: {kind}]"));
            }
        }
    }

    if parts.is_empty() {
        "(no output)".to_string()
    } else {
        parts.join("\n")
    }
}
