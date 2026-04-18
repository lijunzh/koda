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
        let tools = ToolRegistry::with_trust(
            project_root.clone(),
            config.max_context_tokens,
            config.trust,
        );
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
            &env,
            commands,
            &tools.skill_registry,
            // MCP manager isn't attached yet at construction time —
            // rebuild_system_prompt() picks up server instructions later (#922).
            &[],
        );

        Ok(Self {
            project_root,
            tools,
            tool_defs,
            system_prompt,
        })
    }

    /// Rebuild the system prompt from the agent's current skill registry.
    ///
    /// Call this after injecting additional skills (e.g. `inject_builtin_skills`)
    /// so the rebuilt prompt includes all available skills in the `## Skills` section.
    pub fn rebuild_system_prompt(&mut self, config: &KodaConfig, commands: &[(&str, &str)]) {
        let semantic_memory = memory::load(&self.project_root).unwrap_or_default();
        let env = crate::prompt::EnvironmentInfo {
            project_root: &self.project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        // Pull MCP server instructions if a manager is attached and isn't
        // currently locked. We use try_read() (non-blocking) so a stuck MCP
        // server can never wedge prompt rebuilds (#922).
        let mcp_instructions = self
            .tools
            .mcp_manager()
            .and_then(|mgr| mgr.try_read().ok().map(|g| g.server_instructions()))
            .unwrap_or_default();
        self.system_prompt = crate::prompt::build_system_prompt(
            &config.system_prompt,
            &semantic_memory,
            &config.agents_dir,
            &env,
            commands,
            &self.tools.skill_registry,
            &mcp_instructions,
        );
    }

    /// Compact MCP status for the TUI status bar.
    ///
    /// Returns `None` if no MCP servers are configured.
    pub fn mcp_status_bar_info(&self) -> Option<crate::mcp::manager::McpStatusBarInfo> {
        let mgr = self.tools.mcp_manager()?;
        let guard = mgr.try_read().ok()?;
        guard.status_bar_summary()
    }
}
