//! KodaAgent — shared, immutable agent resources.
//!
//! Holds everything that's constant across turns within a session:
//! tools, system prompt, MCP registry, project root. Shareable via `Arc`
//! for parallel sub-agents.
//!
//! Note: `KodaConfig` is NOT stored here because the REPL allows
//! switching models and providers mid-session. Config lives on the
//! caller side and is passed to `KodaSession` per-turn.

use crate::config::KodaConfig;
use crate::mcp::McpRegistry;
use crate::providers::ToolDefinition;
use crate::tools::ToolRegistry;
use crate::{inference, memory};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// MCP server connection status entry (for client rendering).
pub type McpStatus = (String, Result<usize, String>);

/// Shared agent resources. Immutable after construction.
///
/// Create once, share via `Arc<KodaAgent>` across sessions and sub-agents.
pub struct KodaAgent {
    pub project_root: PathBuf,
    pub tools: ToolRegistry,
    pub tool_defs: Vec<ToolDefinition>,
    pub system_prompt: String,
    pub mcp_registry: Arc<RwLock<McpRegistry>>,
    /// MCP server connection results from init (for client rendering).
    pub mcp_statuses: Vec<McpStatus>,
}

impl KodaAgent {
    /// Build a new agent from config and project root.
    ///
    /// Initializes tools, MCP servers, system prompt, and tool definitions.
    pub async fn new(config: &KodaConfig, project_root: PathBuf) -> Result<Self> {
        let mcp_registry = Arc::new(RwLock::new(McpRegistry::new()));
        let mcp_statuses;
        {
            let mut mcp = mcp_registry.write().await;
            mcp_statuses = mcp.start_from_config(&project_root).await;
        }

        let tools = ToolRegistry::new(project_root.clone()).with_mcp_registry(mcp_registry.clone());
        let tool_defs = tools.get_definitions(&config.allowed_tools);

        let semantic_memory = memory::load(&project_root)?;
        let system_prompt = inference::build_system_prompt(
            &config.system_prompt,
            &semantic_memory,
            &config.agents_dir,
            &tool_defs,
        );

        Ok(Self {
            project_root,
            tools,
            tool_defs,
            system_prompt,
            mcp_registry,
            mcp_statuses,
        })
    }
}
