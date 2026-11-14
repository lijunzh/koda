//! Multi-server connection manager — owns all MCP clients.
//!
//! `McpManager` is the single entry point for the rest of koda-core.
//! It starts servers in the background, discovers tools, and routes tool
//! calls to the right server. Each session has its own.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::watch;

use super::client::{McpClient, McpClientStatus};
use super::config::{self, McpServerConfig};
use super::tool_bridge::{McpToolAnnotations, parse_mcp_tool_name};
use crate::db::Database;
use crate::providers::ToolDefinition;
use crate::tools::ToolEffect;

/// A session's MCP manager, shared by its turns and the host that reports on
/// it.
pub type SharedMcpManager = Arc<tokio::sync::RwLock<McpManager>>;

/// Told when a server's startup changes, for the
/// `mcpServer/startupStatus/updated` notification.
pub type McpStatusFn = Arc<dyn Fn(McpStartupUpdate) + Send + Sync>;

/// One server's startup, as it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStartupUpdate {
    /// The server.
    pub server: String,
    /// Where its startup stands.
    pub status: McpStartupStatus,
}

/// Where a server's startup stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpStartupStatus {
    /// Starting.
    Starting,
    /// Started; its tools are offered.
    Ready,
    /// It failed to start, for this reason.
    Failed(String),
    /// Stopped before it had started.
    Cancelled,
}

/// Manager for all MCP server connections.
///
/// Owns the set of `McpClient` instances and provides a unified interface
/// for tool discovery and execution. Servers start in the background, as
/// Codex's do ([`Self::start_server`]): a turn waits for those still
/// starting ([`Self::wait_for_startup`]), and opening a session waits only
/// for the required ones.
pub struct McpManager {
    /// Connected (or attempted) clients, keyed by server name. A server
    /// still starting has a stand-in with no tools.
    clients: HashMap<String, McpClient>,

    /// Cached tool annotations for classify_tool lookups.
    /// Keyed by qualified tool name (`server__tool`).
    annotations: HashMap<String, McpToolAnnotations>,

    /// The servers' configs, to tell what a reload changes.
    configs: HashMap<String, McpServerConfig>,

    /// Servers whose startup is under way, with the startup's number. A
    /// startup that finishes after a newer one began, or after its server
    /// was stopped, is discarded.
    startups: HashMap<String, u64>,

    /// The last startup's number.
    generation: u64,

    /// Bumped whenever a startup ends, for those waiting on one.
    changed: watch::Sender<u64>,

    /// Told when a server's startup changes.
    on_status: Option<McpStatusFn>,
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
            configs: HashMap::new(),
            startups: HashMap::new(),
            generation: 0,
            changed: watch::channel(0).0,
            on_status: None,
        }
    }

    /// The servers a session starts: the stored ones, plus this run's from
    /// `--mcp-config`. A run's server replaces a stored one of the same name
    /// for this run only; nothing is written back.
    pub async fn configured(
        db: &Database,
        session_servers: &HashMap<String, McpServerConfig>,
    ) -> Result<HashMap<String, McpServerConfig>> {
        let mut configs = config::load_mcp_configs(db).await?;
        for (name, server) in session_servers {
            if configs.insert(name.clone(), server.clone()).is_some() {
                tracing::info!(server = %name, "--mcp-config replaces the stored server for this run");
            }
        }
        Ok(configs)
    }

    /// Who is told when a server's startup changes.
    pub fn set_status_fn(&mut self, on_status: McpStatusFn) {
        self.on_status = Some(on_status);
    }

    fn report(&self, server: &str, status: McpStartupStatus) {
        if let Some(on_status) = &self.on_status {
            on_status(McpStartupUpdate {
                server: server.to_string(),
                status,
            });
        }
    }

    fn purge_annotations(&mut self, name: &str) {
        self.annotations.retain(|k, _| {
            parse_mcp_tool_name(k)
                .map(|(s, _)| s != name)
                .unwrap_or(true)
        });
    }

    /// Start every server in `configs` in the background.
    pub async fn start_all(this: &SharedMcpManager, configs: HashMap<String, McpServerConfig>) {
        if !configs.is_empty() {
            tracing::info!(
                count = configs.len(),
                servers = ?configs.keys().collect::<Vec<_>>(),
                "starting MCP servers"
            );
        }
        for (name, config) in configs {
            Self::start_server(this, name, config).await;
        }
    }

    /// Start server `name` in the background, replacing one of that name.
    /// Until it has started it is listed as starting, with no tools.
    pub async fn start_server(this: &SharedMcpManager, name: String, config: McpServerConfig) {
        let (generation, old) = {
            let mut manager = this.write().await;
            manager.generation += 1;
            let generation = manager.generation;
            manager.startups.insert(name.clone(), generation);
            manager.configs.insert(name.clone(), config.clone());
            manager.purge_annotations(&name);
            let old = manager.clients.insert(
                name.clone(),
                McpClient::starting(name.clone(), config.clone()),
            );
            manager.report(&name, McpStartupStatus::Starting);
            (generation, old)
        };
        if let Some(mut old) = old {
            old.disconnect().await;
        }
        let this = Arc::clone(this);
        tokio::spawn(async move {
            let mut client = McpClient::new(name.clone(), config);
            let result = client.connect().await;
            let mut manager = this.write().await;
            if manager.startups.get(&name) != Some(&generation) {
                // Stopped or restarted meanwhile.
                drop(manager);
                client.disconnect().await;
                return;
            }
            manager.startups.remove(&name);
            for tool in client.tools() {
                manager
                    .annotations
                    .insert(tool.definition.name.clone(), tool.annotations.clone());
            }
            let status = match result {
                Ok(()) => McpStartupStatus::Ready,
                Err(e) => McpStartupStatus::Failed(format!("{e:#}")),
            };
            manager.clients.insert(name.clone(), client);
            manager.report(&name, status);
            manager.changed.send_modify(|n| *n += 1);
        });
    }

    /// Stop server `name` and forget it. Returns whether there was one.
    pub async fn stop_server(this: &SharedMcpManager, name: &str) -> bool {
        let client = {
            let mut manager = this.write().await;
            manager.configs.remove(name);
            let was_starting = manager.startups.remove(name).is_some();
            manager.purge_annotations(name);
            let client = manager.clients.remove(name);
            if was_starting {
                manager.report(name, McpStartupStatus::Cancelled);
                manager.changed.send_modify(|n| *n += 1);
            }
            client
        };
        match client {
            Some(mut client) => {
                client.disconnect().await;
                true
            }
            None => false,
        }
    }

    /// Bring the servers in line with `desired`, as Codex's
    /// `config/mcpServer/reload` does: stop those no longer configured,
    /// start new ones, and restart those whose config changed. Koda also
    /// restarts those that failed, so a reload is how a failed server is
    /// tried again.
    pub async fn reconcile(this: &SharedMcpManager, desired: HashMap<String, McpServerConfig>) {
        let (stop, start) = {
            let manager = this.read().await;
            let stop: Vec<String> = manager
                .configs
                .keys()
                .filter(|name| !desired.contains_key(*name))
                .cloned()
                .collect();
            let start: Vec<(String, McpServerConfig)> = desired
                .into_iter()
                .filter(|(name, config)| {
                    manager.configs.get(name) != Some(config)
                        || manager
                            .clients
                            .get(name)
                            .is_some_and(|c| c.status() == McpClientStatus::Failed)
                })
                .collect();
            (stop, start)
        };
        for name in stop {
            Self::stop_server(this, &name).await;
        }
        for (name, config) in start {
            Self::start_server(this, name, config).await;
        }
    }

    /// Wait until no server is starting, or, with `only`, none of those.
    /// Each startup is bounded by its server's `startup_timeout_sec`.
    pub async fn wait_for_startup(this: &SharedMcpManager, only: Option<&[String]>) {
        let mut changed = this.read().await.changed.subscribe();
        loop {
            {
                let manager = this.read().await;
                let waiting = manager
                    .startups
                    .keys()
                    .any(|name| only.is_none_or(|only| only.contains(name)));
                if !waiting {
                    return;
                }
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    /// Wait for the required servers, then say which failed: `name: why`
    /// for each. Codex's session start waits for these the same way.
    pub async fn required_failures(this: &SharedMcpManager) -> Vec<String> {
        let required: Vec<String> = {
            let manager = this.read().await;
            manager
                .configs
                .iter()
                .filter(|(_, config)| config.required)
                .map(|(name, _)| name.clone())
                .collect()
        };
        if required.is_empty() {
            return Vec::new();
        }
        Self::wait_for_startup(this, Some(&required)).await;
        let manager = this.read().await;
        let mut failed: Vec<String> = required
            .iter()
            .filter_map(|name| {
                let client = manager.clients.get(name)?;
                (client.status() != McpClientStatus::Connected).then(|| {
                    format!(
                        "{name}: {}",
                        client.last_error().unwrap_or("it didn't start")
                    )
                })
            })
            .collect();
        failed.sort();
        failed
    }

    /// Get all discovered tool definitions across all connected servers.
    pub fn all_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.clients
            .values()
            .filter(|c| c.status() == McpClientStatus::Connected)
            .flat_map(|c| c.tools().iter().map(|t| t.definition.clone()))
            .collect()
    }

    /// Server-provided instructions for every connected MCP server that
    /// returned a non-empty `instructions` field during `initialize` (#922).
    ///
    /// Returns `(server_name, instructions)` pairs sorted by server name for
    /// stable prompt rendering. Empty when no MCP server provided guidance —
    /// non-MCP users pay zero tokens for the missing block.
    pub fn server_instructions(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .clients
            .iter()
            .filter(|(_, c)| c.status() == McpClientStatus::Connected)
            .filter_map(|(name, c)| {
                c.instructions()
                    .map(|instr| (name.clone(), instr.to_string()))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
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

        // Convert CallToolResult content to a string, marked as data: an
        // MCP server's text, error text included, isn't the user's.
        let output = crate::untrusted::wrap(qualified_name, &call_tool_result_to_string(&result));

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
                tools: c
                    .tools()
                    .iter()
                    .map(|t| McpToolSummary {
                        name: t.original_name.clone(),
                        description: server_description(c.name(), &t.definition.description),
                    })
                    .collect(),
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
        self.clients.clear();
        self.configs.clear();
        // A startup still under way is discarded when it ends.
        self.startups.clear();
        self.changed.send_modify(|n| *n += 1);
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

        self.startups.remove(&name);
        self.configs.insert(name.clone(), config.clone());
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
        self.configs.remove(name);
        self.startups.remove(name);
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
    /// The tools, by their names on the server.
    pub tools: Vec<McpToolSummary>,
    /// Last error message (if failed).
    pub error: Option<String>,
}

/// One of a server's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolSummary {
    /// Its name on the server.
    pub name: String,
    /// What it does, as the server says.
    pub description: Option<String>,
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

/// A tool's description as its server gave it: without the `[MCP:<server>]`
/// mark Koda adds for the model, and `None` when the server gave none.
fn server_description(server: &str, description: &str) -> Option<String> {
    let description = description
        .strip_prefix(&format!("[MCP:{server}] "))
        .unwrap_or(description);
    (description != "MCP tool (no description)" && !description.is_empty())
        .then(|| description.to_string())
}

/// Convert MCP CallToolResult content into a plain string.
///
/// Text content is concatenated. Non-text content (images, blobs) is
/// described inline so the LLM knows something was returned.
fn call_tool_result_to_string(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();

    for content in &result.content {
        match content {
            rmcp::model::ContentBlock::Text(text) => {
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
            required: false,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn call_tool_on_nonexistent_server_returns_err() {
        let mgr = McpManager::new();
        let result = mgr.call_tool("ghost__tool", serde_json::json!({})).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("not found"), "expected 'not found' in: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_nonexistent_server_returns_false() {
        let mut mgr = McpManager::new();
        assert!(!mgr.remove_server("ghost").await);
    }

    // ── add_server: name collision ──────────────────────────────────

    /// When `add_server` is called with a name that already exists the old
    /// client and its annotations are purged BEFORE the new connect attempt.
    /// Even if the new connect fails (dummy `false` command), the stale
    /// annotations from the previous server must be gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        use rmcp::model::{CallToolResult, ContentBlock};
        let result = CallToolResult::success(vec![
            ContentBlock::text("hello"),
            ContentBlock::text("world"),
        ]);
        assert_eq!(call_tool_result_to_string(&result), "hello\nworld");
    }

    #[test]
    fn result_to_string_empty_content() {
        let result = rmcp::model::CallToolResult::success(vec![]);
        assert_eq!(call_tool_result_to_string(&result), "(no output)");
    }

    #[test]
    fn result_to_string_non_text_content() {
        use rmcp::model::{CallToolResult, ContentBlock};
        // Image content is non-text — should produce a descriptive placeholder.
        let result = CallToolResult::success(vec![ContentBlock::image("iVBOR", "image/png")]);
        let output = call_tool_result_to_string(&result);
        assert!(
            output.contains("non-text content"),
            "expected non-text placeholder in: {output}"
        );
    }

    // ── Lifecycle & state-projection coverage (#896) ─────────────────────────
    //
    // Earlier tests cover the *purge* side of remove/reconnect/add. These
    // cover the projection methods (status_summary, all_tool_definitions,
    // classify_tool, is_empty, connected_count) and the multi-server
    // isolation guarantees that prevent cross-server bleed.

    use super::super::client::DiscoveredTool;

    /// Build a discovered tool with the given qualified name and read-only hint.
    fn fake_tool(qualified_name: &str, read_only: Option<bool>) -> DiscoveredTool {
        let original = qualified_name.split("__").nth(1).unwrap_or(qualified_name);
        DiscoveredTool {
            definition: ToolDefinition {
                name: qualified_name.into(),
                description: "fake test tool".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            annotations: McpToolAnnotations {
                read_only_hint: read_only,
                destructive_hint: None,
            },
            original_name: original.into(),
        }
    }

    /// Build a connected client with the given seeded tools.
    fn connected_client_with_tools(name: &str, tools: Vec<DiscoveredTool>) -> McpClient {
        let mut c = McpClient::new(name.into(), dummy_config());
        c.set_status_for_test(McpClientStatus::Connected);
        c.set_tools_for_test(tools);
        c
    }

    // ── shutdown ────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_clears_all_annotations() {
        let mut mgr = McpManager::new();
        // Two clients, each contributing tools/annotations.
        let c1 = connected_client_with_tools("alpha", vec![fake_tool("alpha__read", Some(true))]);
        let c2 = connected_client_with_tools("beta", vec![fake_tool("beta__write", Some(false))]);
        mgr.insert_client_for_test(c1);
        mgr.insert_client_for_test(c2);

        assert!(mgr.has_tool("alpha__read"));
        assert!(mgr.has_tool("beta__write"));

        mgr.shutdown().await;

        assert!(
            !mgr.has_tool("alpha__read") && !mgr.has_tool("beta__write"),
            "shutdown must purge ALL cached annotations"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_on_empty_manager_is_noop() {
        let mut mgr = McpManager::new();
        // Should not panic; should leave manager empty.
        mgr.shutdown().await;
        assert!(mgr.is_empty());
    }

    // ── status_summary ─────────────────────────────────────────────────

    #[test]
    fn status_summary_reports_per_server_details() {
        let mut mgr = McpManager::new();

        let c1 = connected_client_with_tools(
            "good",
            vec![fake_tool("good__t1", None), fake_tool("good__t2", None)],
        );
        mgr.insert_client_for_test(c1);

        let mut c2 = McpClient::new("bad".into(), dummy_config());
        c2.set_status_for_test(McpClientStatus::Failed);
        c2.set_last_error_for_test(Some("connect timeout".into()));
        mgr.insert_client_for_test(c2);

        let mut summary = mgr.status_summary();
        // Order is HashMap-iteration-defined — normalize for assertion.
        summary.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(summary.len(), 2);

        assert_eq!(summary[0].name, "bad");
        assert_eq!(summary[0].status, McpClientStatus::Failed);
        assert_eq!(summary[0].tool_count, 0);
        assert_eq!(summary[0].error.as_deref(), Some("connect timeout"));

        assert_eq!(summary[1].name, "good");
        assert_eq!(summary[1].status, McpClientStatus::Connected);
        assert_eq!(summary[1].tool_count, 2);
        assert_eq!(summary[1].error, None);
    }

    #[test]
    fn status_summary_empty_returns_empty_vec() {
        let mgr = McpManager::new();
        assert!(mgr.status_summary().is_empty());
    }

    // ── all_tool_definitions ────────────────────────────────────────────

    #[test]
    fn all_tool_definitions_excludes_disconnected_and_failed_clients() {
        let mut mgr = McpManager::new();

        // Connected with 2 tools.
        let connected = connected_client_with_tools(
            "live",
            vec![fake_tool("live__a", None), fake_tool("live__b", None)],
        );
        mgr.insert_client_for_test(connected);

        // Disconnected (default) — even with tools seeded, must be excluded.
        let mut zombie = McpClient::new("zombie".into(), dummy_config());
        zombie.set_tools_for_test(vec![fake_tool("zombie__ghost", None)]);
        mgr.insert_client_for_test(zombie);

        // Failed.
        let mut failed = McpClient::new("broken".into(), dummy_config());
        failed.set_status_for_test(McpClientStatus::Failed);
        failed.set_tools_for_test(vec![fake_tool("broken__nope", None)]);
        mgr.insert_client_for_test(failed);

        let defs = mgr.all_tool_definitions();
        let names: std::collections::HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();

        assert_eq!(
            defs.len(),
            2,
            "only the Connected client's tools may be exposed; got {names:?}"
        );
        assert!(names.contains("live__a") && names.contains("live__b"));
        assert!(!names.contains("zombie__ghost"));
        assert!(!names.contains("broken__nope"));
    }

    // ── classify_tool ──────────────────────────────────────────────────

    #[test]
    fn classify_tool_uses_cached_annotations_when_present() {
        let mut mgr = McpManager::new();
        let c = connected_client_with_tools("db", vec![fake_tool("db__select", Some(true))]);
        mgr.insert_client_for_test(c);

        // Annotation must be cached at insert time.
        assert!(mgr.has_tool("db__select"));

        // Whatever the classifier returns, it must be the SAME as the pure
        // tool_bridge classifier would return for the same annotations —
        // proving the cache is in fact looked up rather than ignored.
        let expected = super::super::tool_bridge::classify_mcp_tool(Some(&McpToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: None,
        }));
        assert_eq!(mgr.classify_tool("db__select"), expected);
    }

    #[test]
    fn classify_tool_unknown_falls_back_to_default() {
        let mgr = McpManager::new();
        let unknown = mgr.classify_tool("does_not__exist");
        let expected = super::super::tool_bridge::classify_mcp_tool(None);
        assert_eq!(
            unknown, expected,
            "unknown tool must use the same default classification as a `None` annotation"
        );
    }

    // ── is_empty / connected_count ─────────────────────────────────────

    #[test]
    fn is_empty_and_connected_count_reflect_state() {
        let mut mgr = McpManager::new();
        assert!(mgr.is_empty());
        assert_eq!(mgr.connected_count(), 0);

        // Add a Disconnected client — not empty, but zero connected.
        let zombie = McpClient::new("zombie".into(), dummy_config());
        mgr.insert_client_for_test(zombie);
        assert!(!mgr.is_empty());
        assert_eq!(mgr.connected_count(), 0);

        // Add a Connected client — connected_count rises to 1.
        let live = connected_client_with_tools("live", vec![]);
        mgr.insert_client_for_test(live);
        assert_eq!(mgr.connected_count(), 1);

        // Add another Connected.
        let live2 = connected_client_with_tools("live2", vec![]);
        mgr.insert_client_for_test(live2);
        assert_eq!(mgr.connected_count(), 2);
    }

    // ── Multi-server isolation ─────────────────────────────────────────

    /// Removing one server must not touch any *other* server's annotations.
    /// Regression guard against a buggy `retain` predicate that over-purges
    /// (e.g., `starts_with(name)` instead of `parse_mcp_tool_name() == name`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_server_does_not_touch_other_servers_annotations() {
        let mut mgr = McpManager::new();

        // Two servers whose names share a prefix — catches the
        // `starts_with` foot-gun: 'db' vs 'db_archive'.
        let s1 = connected_client_with_tools("db", vec![fake_tool("db__select", None)]);
        let s2 =
            connected_client_with_tools("db_archive", vec![fake_tool("db_archive__list", None)]);
        mgr.insert_client_for_test(s1);
        mgr.insert_client_for_test(s2);

        assert!(mgr.has_tool("db__select"));
        assert!(mgr.has_tool("db_archive__list"));

        let removed = mgr.remove_server("db").await;
        assert!(removed);

        assert!(
            !mgr.has_tool("db__select"),
            "removed server's tool must be gone"
        );
        assert!(
            mgr.has_tool("db_archive__list"),
            "sibling server's tool MUST survive removal of a name-prefix-sharing server"
        );
    }

    /// Same prefix-isolation guard, but for `add_server` collisions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn add_server_collision_does_not_touch_prefix_neighbour() {
        let mut mgr = McpManager::new();

        let s1 = connected_client_with_tools("db", vec![fake_tool("db__old", None)]);
        let s2 =
            connected_client_with_tools("db_archive", vec![fake_tool("db_archive__keep", None)]);
        mgr.insert_client_for_test(s1);
        mgr.insert_client_for_test(s2);

        // add_server("db", ...) — connect will fail (dummy `false`), but
        // the purge of "db" annotations must run, while "db_archive" stays.
        let _ = mgr.add_server("db".into(), dummy_config()).await;

        assert!(!mgr.has_tool("db__old"));
        assert!(
            mgr.has_tool("db_archive__keep"),
            "add_server collision must not purge sibling-prefix server's tools"
        );
    }

    // ── server_instructions() (#922) ────────────────────────────────────

    #[test]
    fn server_instructions_empty_when_no_clients() {
        let mgr = McpManager::new();
        assert!(mgr.server_instructions().is_empty());
    }

    #[test]
    fn server_instructions_skips_unconnected_servers() {
        let mut mgr = McpManager::new();
        let mut c = McpClient::new("halfdead".into(), dummy_config());
        c.set_status_for_test(McpClientStatus::Failed);
        c.set_instructions_for_test(Some("will be ignored".into()));
        mgr.insert_client_for_test(c);
        assert!(
            mgr.server_instructions().is_empty(),
            "only Connected servers should contribute instructions"
        );
    }

    #[test]
    fn server_instructions_skips_connected_without_instructions() {
        let mut mgr = McpManager::new();
        let mut c = McpClient::new("silent".into(), dummy_config());
        c.set_status_for_test(McpClientStatus::Connected);
        // No instructions set — server didn't return any.
        mgr.insert_client_for_test(c);
        assert!(mgr.server_instructions().is_empty());
    }

    #[test]
    fn server_instructions_returns_connected_with_instructions_sorted() {
        let mut mgr = McpManager::new();
        // Insert in reverse alpha order to verify sorting.
        for (name, instr) in [
            ("zebra", "Z guidance"),
            ("alpha", "A guidance"),
            ("middle", "M guidance"),
        ] {
            let mut c = McpClient::new(name.into(), dummy_config());
            c.set_status_for_test(McpClientStatus::Connected);
            c.set_instructions_for_test(Some(instr.into()));
            mgr.insert_client_for_test(c);
        }
        let result = mgr.server_instructions();
        assert_eq!(
            result,
            vec![
                ("alpha".to_string(), "A guidance".to_string()),
                ("middle".to_string(), "M guidance".to_string()),
                ("zebra".to_string(), "Z guidance".to_string()),
            ],
            "results must be sorted by server name for stable prompt rendering"
        );
    }
}
