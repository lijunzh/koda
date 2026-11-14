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
    /// Built-in tool definitions, computed once in `new()`. MCP tools are
    /// not here: servers connect later. Use [`Self::turn_tool_defs`] for
    /// what a turn offers the model.
    pub tool_defs: Vec<ToolDefinition>,
    /// Assembled system prompt.
    pub system_prompt: String,
    /// The model [`Self::system_prompt`]'s environment section names. A
    /// turn on another model has its own named instead
    /// ([`Self::system_prompt_for_model`]).
    pub prompt_model: String,
    /// Project memory (instruction files and Koda's notes) as loaded in
    /// `new()`. Each turn reloads it, so changes apply next turn; this copy
    /// is used only when that reload fails.
    pub semantic_memory: String,
}

/// Told to a session in Plan (a headless `--mode plan` run) so it knows
/// before its first call that only reading will work, as Claude Code's
/// plan mode tells its model.
const PLAN_MODE_NOTE: &str = "\n## Read-only session\n\n\
     This session is read-only (plan mode). You can read files, search, run commands that \
     only read, and use tools that only read. Anything that would change a file, run another \
     command or act somewhere else is refused. Don't try it: do what you can by reading, and \
     say in your answer what you would change or do.\n";

/// The base system prompt, plus [`PLAN_MODE_NOTE`] in Plan.
fn system_prompt_for(
    config: &KodaConfig,
    env: &crate::prompt::EnvironmentInfo<'_>,
    commands: &[&str],
    skills: &crate::skills::SkillRegistry,
) -> String {
    let mut prompt =
        crate::prompt::build_system_prompt(&config.system_prompt, env, commands, skills);
    if config.trust == crate::trust::TrustMode::Plan {
        prompt.push_str(PLAN_MODE_NOTE);
    }
    prompt
}

impl KodaAgent {
    /// Build a new agent from config and project root.
    ///
    /// `commands` lists the user-facing slash commands. The TUI passes its
    /// `SLASH_COMMANDS` registry here; everything without a REPL passes `&[]`.
    pub async fn new(
        config: &KodaConfig,
        project_root: PathBuf,
        commands: &[&str],
    ) -> Result<Self> {
        let tools = ToolRegistry::with_trust(
            project_root.clone(),
            config.max_context_tokens,
            config.trust,
        );
        let mut tool_defs = tools.get_definitions(&config.allowed_tools, &config.disallowed_tools);
        // Built-ins only: `turn_tool_defs` adds the MCP tools each turn.
        tool_defs.retain(|d| !crate::mcp::is_mcp_tool_name(&d.name));

        let semantic_memory = memory::load(&project_root)?;
        let env = crate::prompt::EnvironmentInfo {
            project_root: &project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = system_prompt_for(config, &env, commands, &tools.skill_registry);

        Ok(Self {
            project_root,
            tools,
            tool_defs,
            system_prompt,
            prompt_model: config.model.clone(),
            semantic_memory,
        })
    }

    /// Rebuild the system prompt from the agent's current skill registry.
    ///
    /// Call this after injecting additional skills (e.g. `inject_builtin_skills`)
    /// so the rebuilt prompt includes all available skills in the `## Skills` section.
    ///
    /// Note: MCP server instructions are NOT included here — they are composed
    /// per-turn in `KodaSession::run_turn` via `render_mcp_instructions_section`,
    /// because MCP servers may attach after this static prompt is built (#922).
    pub fn rebuild_system_prompt(&mut self, config: &KodaConfig, commands: &[&str]) {
        // Reuse cached memory loaded in `new()` — avoids duplicate disk read
        // and duplicate `Loaded project memory from CLAUDE.md` log line (#1136).
        let env = crate::prompt::EnvironmentInfo {
            project_root: &self.project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        self.system_prompt = system_prompt_for(config, &env, commands, &self.tools.skill_registry);
        self.prompt_model = config.model.clone();
    }

    /// The system prompt for a turn on `model`. One agent serves every turn
    /// of a session, and of sessions sharing it, while the model can change
    /// (`/model`, or a saved choice the host restores after the agent was
    /// built), so the environment section names the turn's own.
    pub fn system_prompt_for_model(&self, model: &str) -> String {
        if model == self.prompt_model {
            return self.system_prompt.clone();
        }
        self.system_prompt.replacen(
            &crate::prompt::model_line(&self.prompt_model),
            &crate::prompt::model_line(model),
            1,
        )
    }

    /// The tools a turn offers the model: the built-ins plus whatever the
    /// connected MCP servers expose right now, under the same allow/deny
    /// lists.
    pub fn turn_tool_defs(&self, config: &KodaConfig) -> Vec<ToolDefinition> {
        let mut defs = self.tool_defs.clone();
        defs.extend(
            self.tools
                .mcp_definitions(&config.allowed_tools, &config.disallowed_tools),
        );
        defs
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

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(prompt: &str, model: &str) -> KodaAgent {
        KodaAgent {
            project_root: PathBuf::from("/tmp"),
            tools: ToolRegistry::new(PathBuf::from("/tmp"), 100_000),
            tool_defs: Vec::new(),
            system_prompt: prompt.to_string(),
            prompt_model: model.to_string(),
            semantic_memory: String::new(),
        }
    }

    #[test]
    fn a_turn_on_another_model_names_its_own() {
        let prompt = format!(
            "# Env\n{}- Platform: macos\n",
            crate::prompt::model_line("old-model")
        );
        let agent = agent(&prompt, "old-model");
        assert_eq!(agent.system_prompt_for_model("old-model"), prompt);
        let switched = agent.system_prompt_for_model("new-model");
        assert!(switched.contains("- Model: new-model\n"), "{switched}");
        assert!(!switched.contains("old-model"), "{switched}");
        assert!(switched.contains("- Platform: macos"), "the rest stays");
    }
}
