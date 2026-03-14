//! KodaAgent — shared, immutable agent resources.
//!
//! Holds everything that's constant across turns within a session:
//! tools, system prompt, project root. Shareable via `Arc`
//! for parallel sub-agents.
//!
//! Note: `KodaConfig` is NOT stored here because the REPL allows
//! switching models and providers mid-session. Config lives on the
//! caller side and is passed to `KodaSession` per-turn.

use crate::config::KodaConfig;
use crate::memory;
use crate::providers::ToolDefinition;
use crate::tools::ToolRegistry;

use anyhow::Result;
use std::path::PathBuf;

/// Shared agent resources. Immutable after construction.
///
/// Create once, share via `Arc<KodaAgent>` across sessions and sub-agents.
pub struct KodaAgent {
    /// Project root directory.
    pub project_root: PathBuf,
    /// Tool registry with all built-in tools.
    pub tools: ToolRegistry,
    /// Pre-computed tool definitions for the LLM.
    pub tool_defs: Vec<ToolDefinition>,
    /// Assembled system prompt.
    pub system_prompt: String,
}

impl KodaAgent {
    /// Build a new agent from config and project root.
    ///
    /// Initializes tools, system prompt, and tool definitions.
    pub async fn new(config: &KodaConfig, project_root: PathBuf) -> Result<Self> {
        let tools = ToolRegistry::new(project_root.clone(), config.max_context_tokens);
        let tool_defs = tools.get_definitions(&config.allowed_tools);

        let semantic_memory = memory::load(&project_root)?;
        let system_prompt = crate::prompt::build_system_prompt(
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
        })
    }
}
