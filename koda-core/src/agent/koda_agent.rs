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
    /// Whether a person answers this session's questions: the prompt's
    /// quick reference, kept for [`Self::rebuild_system_prompt`].
    pub interactive: bool,
}

/// Told to a headless session in Plan (`koda exec --mode plan`) so it
/// knows before its first call that only reading will work, as Claude
/// Code's plan mode tells its model.
const PLAN_MODE_NOTE: &str = "\n## Read-only session\n\n\
     This session is read-only (plan mode). You can read files, search, run commands that \
     only read, and use tools that only read. Anything that would change a file, run another \
     command or act somewhere else is refused. Don't try it: do what you can by reading, and \
     say in your answer what you would change or do.\n";

/// Opens [`PLAN_MODE_NOTE`] and [`PLAN_MODE_INTERACTIVE_NOTE`] alike.
const PLAN_MODE_HEADING: &str = "\n## Read-only session";

/// Told to a session a person answers while it is in Plan, as Codex's
/// plan mode tells its model: explore, ask, then propose a plan in a
/// `<proposed_plan>` block, which the TUI offers to implement.
const PLAN_MODE_INTERACTIVE_NOTE: &str = "\n## Read-only session (plan mode)\n\n\
     You are in plan mode until the user leaves it. Plan the work with the user; don't do it. \
     You can read files, search, run commands that only read and use tools that only \
     read. Anything that would change a file, run another command or act somewhere else is refused. Don't try \
     it, even when asked to do the work: a request to do something is, here, a request to \
     plan it.\n\n\
     First explore: find out what you can from the project rather than asking. Then ask \
     the user about what you can't find out, such as goals, preferences and tradeoffs, \
     one or a few questions at a time, each one that changes the plan. When the plan \
     leaves no decision to whoever implements it, present it once, wrapped in a \
     `<proposed_plan>` block, each tag on a line of its own:\n\n\
     <proposed_plan>\n\
     the plan, in Markdown\n\
     </proposed_plan>\n\n\
     The plan has a title, a short summary, the changes by behaviour or part (naming files \
     only where needed), how to test them, and the assumptions made. Keep it concise. Don't \
     ask whether to go ahead: the user is offered to implement the plan or to keep \
     planning. A revised plan is a complete new `<proposed_plan>` block, and there is at \
     most one per answer.\n";

/// The base system prompt. Plan's note is added per turn
/// ([`KodaAgent::system_prompt_for_turn`]), as the mode can change between
/// turns.
fn system_prompt_for(
    config: &KodaConfig,
    env: &crate::prompt::EnvironmentInfo<'_>,
    interactive: bool,
    skills: &crate::skills::SkillRegistry,
) -> String {
    let mut prompt =
        crate::prompt::build_system_prompt(&config.system_prompt, env, interactive, skills);
    if config.shell_search {
        prompt = crate::shell_search::adapt_prompt(&prompt);
    }
    if let Some(text) = &config.developer_instructions {
        prompt.push_str("\n\n## Developer instructions\n\n");
        prompt.push_str(text);
        prompt.push('\n');
    }
    prompt
}

impl KodaAgent {
    /// Build a new agent from config and project root.
    ///
    /// `interactive` is whether a person answers the session's questions,
    /// which adds the prompt's quick reference
    /// ([`crate::prompt::build_system_prompt`]).
    pub async fn new(
        config: &KodaConfig,
        project_root: PathBuf,
        interactive: bool,
    ) -> Result<Self> {
        let mut tools = ToolRegistry::with_trust(
            project_root.clone(),
            config.max_context_tokens,
            config.trust,
        );
        tools.max_goal_token_budget = config.max_goal_token_budget;
        // The session's `WebSearch` budget, as Claude Code's: it refills in
        // a session a person attends and not in one nobody does.
        let refills = config
            .web_search_refills_per_hour
            .unwrap_or(if interactive {
                crate::tools::web_search::DEFAULT_WEB_SEARCH_REFILLS_PER_HOUR
            } else {
                0
            });
        let tools = tools.with_web_search_budget(std::sync::Arc::new(
            crate::tools::web_search::WebSearchBudget::new(
                config.max_web_searches_per_session,
                refills,
            ),
        ));
        let mut tool_defs = tools.get_definitions(&config.allowed_tools, &config.disallowed_tools);
        // Built-ins only: `turn_tool_defs` adds the MCP tools each turn.
        tool_defs.retain(|d| !crate::mcp::is_mcp_tool_name(&d.name));

        let semantic_memory = memory::load(&project_root)?;
        let env = crate::prompt::EnvironmentInfo {
            project_root: &project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        let system_prompt = system_prompt_for(config, &env, interactive, &tools.skill_registry);

        Ok(Self {
            project_root,
            tools,
            tool_defs,
            system_prompt,
            prompt_model: config.model.clone(),
            semantic_memory,
            interactive,
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
    pub fn rebuild_system_prompt(&mut self, config: &KodaConfig) {
        // Reuse cached memory loaded in `new()` — avoids duplicate disk read
        // and duplicate `Loaded project memory from CLAUDE.md` log line (#1136).
        let env = crate::prompt::EnvironmentInfo {
            project_root: &self.project_root,
            model: &config.model,
            platform: std::env::consts::OS,
        };
        self.system_prompt =
            system_prompt_for(config, &env, self.interactive, &self.tools.skill_registry);
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

    /// The system prompt for a turn on `model` in `mode`: in Plan, with
    /// the note saying what that means, as Codex tells its model when its
    /// plan mode starts. A session a person answers is told to plan with
    /// them and to propose the plan in a `<proposed_plan>` block; a
    /// headless one only that it can't change anything.
    pub fn system_prompt_for_turn(&self, model: &str, mode: crate::trust::TrustMode) -> String {
        let mut prompt = self.system_prompt_for_model(model);
        if mode == crate::trust::TrustMode::Plan && !prompt.contains(PLAN_MODE_HEADING) {
            prompt.push_str(if self.interactive {
                PLAN_MODE_INTERACTIVE_NOTE
            } else {
                PLAN_MODE_NOTE
            });
        }
        prompt
    }

    /// The tools a turn offers the model: the built-ins plus whatever the
    /// connected MCP servers expose right now, under the same allow/deny
    /// lists.
    pub fn turn_tool_defs(&self, config: &KodaConfig) -> Vec<ToolDefinition> {
        let mut defs = self.tool_defs.clone();
        if config.shell_search {
            crate::shell_search::adapt_tool_defs(&mut defs);
        }
        if !config.goals {
            defs.retain(|d| !crate::goal::GOAL_TOOL_NAMES.contains(&d.name.as_str()));
        } else if config.goal_judge == koda_config::GoalJudge::Evaluator {
            // The evaluator decides when the goal is done, so the working
            // model can't mark it, as in Claude Code. The user pauses it.
            defs.retain(|d| d.name != "UpdateGoal");
        }
        defs.extend(
            self.tools
                .mcp_definitions(&config.allowed_tools, &config.disallowed_tools),
        );
        defs
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
            interactive: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_web_search_budget_refills_only_where_someone_attends() {
        use crate::config::{KodaConfig, ProviderType};
        let dir = tempfile::tempdir().unwrap();
        let mut config = KodaConfig::default_for_testing(ProviderType::Anthropic);
        let refusal = |agent: &KodaAgent| agent.tools.web_search_budget().refusal();

        let attended = KodaAgent::new(&config, dir.path().into(), true)
            .await
            .unwrap();
        assert!(
            refusal(&attended).contains(
                "limit: 200 WebSearch calls shared by every agent in \
                 this session; the budget refills at about 100 calls per hour"
            ),
            "{}",
            refusal(&attended)
        );
        let headless = KodaAgent::new(&config, dir.path().into(), false)
            .await
            .unwrap();
        assert!(
            refusal(&headless).contains("it does not refill"),
            "{}",
            refusal(&headless)
        );

        config.max_web_searches_per_session = 5;
        config.web_search_refills_per_hour = Some(7);
        let set = KodaAgent::new(&config, dir.path().into(), false)
            .await
            .unwrap();
        assert!(
            refusal(&set).contains(
                "limit: 5 WebSearch calls shared by every agent in this \
                 session; the budget refills at about 7 calls per hour"
            ),
            "{}",
            refusal(&set)
        );
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

    #[test]
    fn a_plan_turn_is_told_it_plans() {
        use crate::trust::TrustMode;
        let mut agent = agent("# Base\n", "m");
        assert_eq!(
            agent.system_prompt_for_turn("m", TrustMode::Safe),
            "# Base\n"
        );
        assert_eq!(
            agent.system_prompt_for_turn("m", TrustMode::Auto),
            "# Base\n"
        );
        let headless = agent.system_prompt_for_turn("m", TrustMode::Plan);
        assert!(headless.ends_with(PLAN_MODE_NOTE), "{headless}");
        assert!(!headless.contains("<proposed_plan>"), "{headless}");

        agent.interactive = true;
        let answered = agent.system_prompt_for_turn("m", TrustMode::Plan);
        assert!(answered.ends_with(PLAN_MODE_INTERACTIVE_NOTE), "{answered}");
        assert!(answered.contains("\n<proposed_plan>\n"), "{answered}");
        // Back in Safe, the next turn has no note.
        assert_eq!(
            agent.system_prompt_for_turn("m", TrustMode::Safe),
            "# Base\n"
        );
    }
}
