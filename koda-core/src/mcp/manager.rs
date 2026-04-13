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

    /// Compact status for the TUI status bar.
    ///
    /// Returns `None` if no MCP servers are configured (hide the indicator).
    pub fn status_bar_summary(&self) -> Option<McpStatusBarInfo> {
        if self.clients.is_empty() {
            return None;
        }
        let total = self.clients.len();
        let connected = self
            .clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Connected)
            .count();
        let failed = self
            .clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Failed)
            .count();
        Some(McpStatusBarInfo {
            connected,
            failed,
            total,
        })
    }

    /// Reconnect a specific server by name.
    ///
    /// Disconnects the existing client (if any) and reconnects using the
    /// stored config. Returns the number of tools discovered on success.
    pub async fn reconnect_server(&mut self, name: &str) -> Result<usize> {
        let client = self
            .clients
            .get_mut(name)
            .with_context(|| format!("MCP server '{name}' not found"))?;

        // Disconnect first.
        client.disconnect().await;

        // Purge old annotations for this server.
        let prefix = format!("{name}__");
        self.annotations.retain(|k, _| !k.starts_with(&prefix));

        // Reconnect.
        client.connect().await?;

        // Re-cache annotations.
        for tool in client.tools() {
            self.annotations
                .insert(tool.definition.name.clone(), tool.annotations.clone());
        }

        Ok(client.tools().len())
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

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Insert a pre-built client directly (test-only).
    #[cfg(feature = "test-support")]
    pub fn insert_client_for_test(&mut self, client: McpClient) {
        let name = client.name().to_string();
        for tool in client.tools() {
            self.annotations
                .insert(tool.definition.name.clone(), tool.annotations.clone());
        }
        self.clients.insert(name, client);
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

/// Compact MCP status for the TUI status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpStatusBarInfo {
    /// Number of connected servers.
    pub connected: usize,
    /// Number of failed servers.
    pub failed: usize,
    /// Total configured servers.
    pub total: usize,
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

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::config::{McpServerConfig, McpTransport};
    use super::*;
    use std::collections::HashMap;

    /// Build a dummy stdio config (won't actually connect).
    fn dummy_config() -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Stdio {
                command: "false".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            startup_timeout_sec: 1,
            tool_timeout_sec: 1,
            enabled_tools: None,
            disabled_tools: None,
        }
    }

    /// Build a manager with one disconnected and one failed client.
    fn manager_with_mixed_clients() -> McpManager {
        let mut mgr = McpManager::new();

        let c1 = McpClient::new("server_a".into(), dummy_config());
        // c1 stays Disconnected (default)
        mgr.insert_client_for_test(c1);

        let mut c2 = McpClient::new("server_b".into(), dummy_config());
        c2.set_status_for_test(McpClientStatus::Failed);
        c2.set_last_error_for_test(Some("connection refused".into()));
        mgr.insert_client_for_test(c2);

        mgr
    }

    // ── call_tool on disconnected server ───────────────────────────────

    #[tokio::test]
    async fn call_tool_on_disconnected_server_returns_err() {
        let mgr = manager_with_mixed_clients();
        let result = mgr
            .call_tool("server_a__some_tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not connected"),
            "expected 'not connected' in: {msg}"
        );
    }

    #[tokio::test]
    async fn call_tool_on_nonexistent_server_returns_err() {
        let mgr = McpManager::new();
        let result = mgr.call_tool("ghost__tool", serde_json::json!({})).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "expected 'not found' in: {msg}");
    }

    #[tokio::test]
    async fn call_tool_with_invalid_name_returns_err() {
        let mgr = McpManager::new();
        let result = mgr.call_tool("no_separator", serde_json::json!({})).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid MCP tool name"),
            "expected parse error in: {msg}"
        );
    }

    // ── remove_server purges annotation cache ─────────────────────────

    #[tokio::test]
    async fn remove_server_purges_annotations() {
        let mut mgr = McpManager::new();
        let c = McpClient::new("myserver".into(), dummy_config());
        mgr.insert_client_for_test(c);

        // Manually insert an annotation as if the server had tools.
        mgr.annotations
            .insert("myserver__list_files".into(), McpToolAnnotations::default());
        assert!(mgr.has_tool("myserver__list_files"));

        let removed = mgr.remove_server("myserver").await;
        assert!(removed);
        assert!(
            !mgr.has_tool("myserver__list_files"),
            "annotation cache must be purged after remove"
        );
    }

    #[tokio::test]
    async fn remove_nonexistent_server_returns_false() {
        let mut mgr = McpManager::new();
        assert!(!mgr.remove_server("ghost").await);
    }

    // ── add_server: name collision ──────────────────────────────────

    /// When `add_server` is called with a name that already exists the old
    /// client and its annotations are purged BEFORE the new connect attempt.
    /// Even if the new connect fails (dummy `false` command), the stale
    /// annotations from the previous server must be gone.
    #[tokio::test]
    async fn add_server_collision_purges_old_annotations() {
        let mut mgr = McpManager::new();

        // Seed an existing client with a fake annotation.
        let c = McpClient::new("myserver".into(), dummy_config());
        mgr.insert_client_for_test(c);
        mgr.annotations
            .insert("myserver__old_tool".into(), McpToolAnnotations::default());
        assert!(mgr.has_tool("myserver__old_tool"), "precondition");

        // add_server with the same name — connect will fail (command `false`).
        let result = mgr.add_server("myserver".into(), dummy_config()).await;
        // We don’t care whether connect succeeded; the annotation purge
        // must have happened regardless.
        let _ = result;

        assert!(
            !mgr.has_tool("myserver__old_tool"),
            "stale annotation must be purged on collision, even if reconnect fails"
        );
    }

    // ── reconnect_server: stale annotations ─────────────────────────

    /// `reconnect_server` must purge annotations for the given server
    /// before attempting the reconnect.  Even if the reconnect fails
    /// (dummy `false` command), stale entries must be gone.
    #[tokio::test]
    async fn reconnect_server_purges_stale_annotations() {
        let mut mgr = McpManager::new();

        let c = McpClient::new("db".into(), dummy_config());
        mgr.insert_client_for_test(c);
        mgr.annotations
            .insert("db__query".into(), McpToolAnnotations::default());
        mgr.annotations
            .insert("db__insert".into(), McpToolAnnotations::default());
        assert!(mgr.has_tool("db__query"), "precondition");

        // Reconnect — connect will fail (command `false`), but annotations
        // must be purged before the attempt.
        let _ = mgr.reconnect_server("db").await;

        assert!(
            !mgr.has_tool("db__query"),
            "db__query annotation must be purged after reconnect attempt"
        );
        assert!(
            !mgr.has_tool("db__insert"),
            "db__insert annotation must be purged after reconnect attempt"
        );
    }

    #[tokio::test]
    async fn reconnect_server_nonexistent_returns_err() {
        let mut mgr = McpManager::new();
        let result = mgr.reconnect_server("ghost").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ── status_bar_summary states ─────────────────────────────────────

    #[test]
    fn status_bar_summary_empty_returns_none() {
        let mgr = McpManager::new();
        assert!(mgr.status_bar_summary().is_none());
    }

    #[test]
    fn status_bar_summary_all_connected() {
        let mut mgr = McpManager::new();
        let mut c1 = McpClient::new("a".into(), dummy_config());
        c1.set_status_for_test(McpClientStatus::Connected);
        mgr.insert_client_for_test(c1);

        let mut c2 = McpClient::new("b".into(), dummy_config());
        c2.set_status_for_test(McpClientStatus::Connected);
        mgr.insert_client_for_test(c2);

        let info = mgr.status_bar_summary().unwrap();
        assert_eq!(info.connected, 2);
        assert_eq!(info.failed, 0);
        assert_eq!(info.total, 2);
    }

    #[test]
    fn status_bar_summary_partial_failure() {
        let mgr = manager_with_mixed_clients();
        let info = mgr.status_bar_summary().unwrap();
        assert_eq!(info.connected, 0);
        assert_eq!(info.failed, 1);
        assert_eq!(info.total, 2);
    }

    #[test]
    fn status_bar_summary_all_failed() {
        let mut mgr = McpManager::new();
        let mut c = McpClient::new("bad".into(), dummy_config());
        c.set_status_for_test(McpClientStatus::Failed);
        mgr.insert_client_for_test(c);

        let info = mgr.status_bar_summary().unwrap();
        assert_eq!(info.connected, 0);
        assert_eq!(info.failed, 1);
        assert_eq!(info.total, 1);
    }

    // ── call_tool_result_to_string ────────────────────────────────────

    #[test]
    fn result_to_string_text_content() {
        use rmcp::model::{CallToolResult, Content};
        let result = CallToolResult::success(vec![Content::text("hello"), Content::text("world")]);
        assert_eq!(call_tool_result_to_string(&result), "hello\nworld");
    }

    #[test]
    fn result_to_string_empty_content() {
        let result = rmcp::model::CallToolResult::success(vec![]);
        assert_eq!(call_tool_result_to_string(&result), "(no output)");
    }

    #[test]
    fn result_to_string_non_text_content() {
        use rmcp::model::{CallToolResult, Content};
        // Image content is non-text — should produce a descriptive placeholder.
        let result = CallToolResult::success(vec![Content::image("iVBOR", "image/png")]);
        let output = call_tool_result_to_string(&result);
        assert!(
            output.contains("non-text content"),
            "expected non-text placeholder in: {output}"
        );
    }
}
