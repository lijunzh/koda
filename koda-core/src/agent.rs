//! KodaAgent — shared, immutable agent resources.
//!
//! Holds everything that's constant across turns within a session:
//! tools, system prompt, project root. Shareable via `Arc`
//! for parallel sub-agents.
//!
//! Note: `KodaConfig` is NOT stored here because the REPL allows
//! switching models and providers mid-session. Config lives on the
//! caller side and is passed to `KodaSession` per-turn.
//!
//! ## Built-in agents
//!
//! Koda ships with these built-in agents (see [`crate::config`] module):
//!
//! | Agent | Purpose | Write access |
//! |---|---|---|
//! | **default** | Main interactive agent with all tools | Yes |
//! | **task** | General-purpose worker for delegated tasks | Yes |
//! | **explore** | Read-only code search specialist | No |
//! | **plan** | Architecture and planning specialist | No |
//! | **verify** | Code review and verification | No |
//!
//! ## Custom agents
//!
//! Define agents as JSON files in `agents/` (project) or `~/.config/koda/agents/` (global):
//!
//! ```json
//! {
//!   "name": "testgen",
//!   "system_prompt": "You are a test generation specialist.",
//!   "model": "gemini-2.5-flash",
//!   "write_access": true,
//!   "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
//! }
//! ```
//!
//! See [`crate::config::AgentConfig`] for all available fields.

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
    /// `commands` is a list of `(name, description)` pairs for user-facing
    /// slash commands.  The CLI passes its `SLASH_COMMANDS` registry here;
    /// sub-agents pass `&[]`.
    pub async fn new(
        config: &KodaConfig,
        project_root: PathBuf,
        commands: &[(&str, &str)],
    ) -> Result<Self> {
        let tools = ToolRegistry::new(project_root.clone(), config.max_context_tokens);
        let tool_defs = tools.get_definitions(&config.allowed_tools, &config.disallowed_tools);

        let semantic_memory = memory::load(&project_root)?;
        let env = crate::prompt::EnvironmentInfo {
            project_root: &project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = crate::prompt::build_system_prompt(
            &config.system_prompt,
            &semantic_memory,
            &config.agents_dir,
            &tool_defs,
            &env,
            commands,
        );

        Ok(Self {
            project_root,
            tools,
            tool_defs,
            system_prompt,
        })
    }
}
