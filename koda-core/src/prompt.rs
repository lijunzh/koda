//! System prompt construction.
//!
//! Builds the system prompt from agent config, memory, and available tools.
//! The prompt is the single source of truth for what the model knows about
//! Koda's capabilities — it is **generated from code**, not a static file.
//!
//! ## Prompt structure
//!
//! The assembled prompt contains (in order):
//!
//! 1. **Base prompt** — from the agent's `system_prompt` field
//! 2. **Behavioral instructions** — `instructions.md` (how to act)
//! 3. **Environment** — working dir, platform, shell, model
//! 4. **Quick Reference** — auto-generated from `SLASH_COMMANDS` + `ToolDefinition`
//! 5. **Sub-agents** — available agents with descriptions and delegation guidance
//! 6. **Skills** — live listing with `when_to_use` hints; model MUST activate before responding
//! 7. **Memory** — project and global learned facts

use std::path::Path;

use crate::skills::SkillRegistry;

/// Runtime environment context injected into the system prompt.
pub struct EnvironmentInfo<'a> {
    /// Project root / working directory.
    pub project_root: &'a Path,
    /// Model identifier (e.g. "claude-sonnet-4-6", "gpt-4o").
    pub model: &'a str,
    /// Platform (e.g. "macos", "linux").
    pub platform: &'a str,
}

/// Build the system prompt with instructions, environment, memory, and tool schemas.
///
/// `commands` is a list of `(name, description)` pairs for user-facing slash
/// commands (e.g. `("/help", "Show this help")`).  Pass `&[]` for sub-agents
/// that don't expose a REPL.
///
/// `skill_registry` is used to build the live `## Skills` section so the model
/// sees every available skill — with its `when_to_use` hint — without needing
/// to call `ListSkills` first.
pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
    env: &EnvironmentInfo<'_>,
    commands: &[(&str, &str)],
    skill_registry: &SkillRegistry,
) -> String {
    let mut prompt = base_prompt.to_string();

    // Behavioral instructions (CC-aligned, #587)
    prompt.push_str("\n\n");
    prompt.push_str(include_str!("instructions.md"));

    // Environment context
    prompt.push_str("\n\n## Environment\n");
    prompt.push_str(&format!(
        "- Working directory: {}\n",
        env.project_root.display()
    ));
    prompt.push_str(&format!("- Platform: {}\n", env.platform));
    if let Ok(shell) = std::env::var("SHELL") {
        prompt.push_str(&format!("- Shell: {}\n", shell));
    }
    prompt.push_str(&format!("- Model: {}\n", env.model));

    // Capabilities quick-reference (generated from code, replaces static capabilities.md)
    prompt.push_str("\n## Koda Quick Reference\n\n");
    prompt.push_str("Refer to this when the user asks \"what can you do?\" or about features.\n");

    // Commands — generated from the registry passed by the CLI
    if !commands.is_empty() {
        prompt.push_str("\n### Commands (user types these in the REPL)\n\n");
        for &(name, desc) in commands {
            prompt.push_str(&format!("- `{name}` — {desc}\n"));
        }
        prompt.push_str("- `Shift+Tab` — cycle approval mode (auto/confirm)\n");
    }

    // Static behavioral guidance (doesn't drift — hardcoded is fine)
    prompt.push_str(
        "\n### Input\n\n\
         - `@file.rs` attaches file context, `@image.png` for multi-modal analysis\n\
         - `Alt+Enter` inserts a newline for multi-line prompts\n\
         - Piped input: `echo \"explain\" | koda` or `koda -p \"prompt\"` for headless/CI\n",
    );
    prompt.push_str(
        "\n### Approval\n\n\
         Two modes (cycle with Shift+Tab): **auto** (default), **confirm**.\n\
         Hotkeys during tool confirmation: `y` approve, `n` reject, `f` feedback, `a` always.\n",
    );
    prompt.push_str(
        "\n### Git Checkpointing\n\n\
         Auto-snapshots working tree before each turn. `/undo` to rollback.\n",
    );

    // Auto-generate tool reference from definitions
    if !tool_defs.is_empty() {
        prompt.push_str("\n### Available Tools\n\n");
        for def in tool_defs {
            // First sentence of description (concise)
            let desc = def
                .description
                .split('.')
                .next()
                .unwrap_or(&def.description);
            prompt.push_str(&format!("- **{}**: {}\n", def.name, desc));
        }
    }

    // Sub-agents — dynamic listing with descriptions
    let available_agents = list_available_agents(agents_dir);
    if !available_agents.is_empty() {
        prompt.push_str("\n\n## Available Sub-Agents\n\n");
        prompt.push_str(
            "Use `InvokeAgent` when the task matches an agent's description below. \
             Do NOT invent agent names that are not listed here.\n\n",
        );
        for (name, desc) in &available_agents {
            if let Some(d) = desc {
                prompt.push_str(&format!("- **{name}** — {d}\n"));
            } else {
                prompt.push_str(&format!("- {name}\n"));
            }
        }
        prompt.push_str(
            "\nWhen to use sub-agents:\n\
             - Complex multi-step tasks where you want to keep your context clean\n\
             - Independent parallel work (launch multiple agents in one response)\n\
             - Research that would fill your context with noise (file contents, grep results)\n\
             \n\
             When NOT to use sub-agents:\n\
             - Simple file reads or 2\u{2013}3 grep queries (overhead > direct execution)\n\
             - Tasks that need user interaction (sub-agents can\u{2019}t ask questions)\n\
             \n\
             Sub-agent results are NOT visible to the user — always summarize key findings.\n",
        );
    } else {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    // Skills — live listing so the model sees every skill upfront, no ListSkills call needed.
    let skills = skill_registry.list();
    if skills.is_empty() {
        prompt.push_str(
            "\n## Skills\n\n\
             No skills are currently available. \
             Add custom skills to `.koda/skills/<name>/SKILL.md`.\n",
        );
    } else {
        prompt.push_str(
            "\n## Skills\n\n\
             Expert instruction modules — zero LLM cost, instant activation via `ActivateSkill`.\n\
             IMPORTANT: If the user's request matches a skill below, you MUST call \
             `ActivateSkill` FIRST — before writing any response. \
             Do not answer from training data when a skill covers the topic.\n\n",
        );
        for meta in &skills {
            // Base: "- **name** — description"
            let mut line = format!("- **{}** — {}", meta.name, meta.description);
            // Append when_to_use if present
            if let Some(wtu) = &meta.when_to_use {
                line.push_str(&format!(" — {wtu}"));
            }
            // Append tool scope hint
            if !meta.allowed_tools.is_empty() {
                line.push_str(&format!(" (Tools: {})", meta.allowed_tools.join(", ")));
            }
            // Append argument hint
            if let Some(hint) = &meta.argument_hint {
                line.push_str(&format!(" `{hint}`"));
            }
            // Mark model-only skills
            if !meta.user_invocable {
                line.push_str(" [model-only]");
            }
            line.push('\n');
            prompt.push_str(&line);
        }
        prompt.push_str(
            "\nCustom skills: `.koda/skills/<name>/SKILL.md` (project) \
             or `~/.config/koda/skills/<name>/SKILL.md` (global).\n",
        );
    }

    // Memory paths
    prompt.push_str(
        "\n## Memory\n\n\
         Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | \
         Global: `~/.config/koda/memory.md`\n",
    );

    // Semantic memory
    if !semantic_memory.is_empty() {
        prompt.push_str(&format!(
            "\n## Project Memory\n\
             The following are learned facts about this project:\n\
             {semantic_memory}"
        ));
    }

    prompt
}

/// Scan the agents/ directory and return available agent names with optional descriptions.
///
/// Returns `(name, Option<description>)` pairs sorted by name.
/// Descriptions come from the `description` field in the agent's JSON config.
/// The default/main agent (`koda`, `default`) is excluded — it is not a sub-agent.
fn list_available_agents(agents_dir: &Path) -> Vec<(String, Option<String>)> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut agents: Vec<(String, Option<String>)> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let name = file_name.strip_suffix(".json")?.to_string();
            // Skip the default agent — it's the main agent, not a sub-agent.
            if name == "koda" || name == "default" {
                return None;
            }
            let description = std::fs::read_to_string(entry.path()).ok().and_then(|json| {
                serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| v["description"].as_str().map(str::to_string))
            });
            Some((name, description))
        })
        .collect();
    agents.sort_by(|a, b| a.0.cmp(&b.0));
    agents
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::SkillRegistry;
    use tempfile::TempDir;

    fn test_env() -> EnvironmentInfo<'static> {
        // Use a leaked path so the reference lives long enough for tests
        let path: &'static Path = Path::new("/test/project");
        EnvironmentInfo {
            project_root: path,
            model: "test-model",
            platform: "test-os",
        }
    }

    #[test]
    fn test_build_system_prompt_no_agents_no_memory() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt(
            "You are helpful.",
            "",
            dir.path(),
            &[],
            &env,
            &[],
            &registry,
        );
        assert!(result.starts_with("You are helpful."));
        assert!(result.contains("Doing Tasks"));
        assert!(result.contains("Koda Quick Reference"));
        assert!(!result.contains("Project Memory"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt(
            "You are helpful.",
            "This is a Rust project.",
            dir.path(),
            &[],
            &env,
            &[],
            &registry,
        );
        assert!(result.contains("Project Memory"));
        assert!(result.contains("Rust project"));
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let tools = vec![crate::providers::ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file. Returns contents.".to_string(),
            parameters: serde_json::json!({}),
        }];
        let result = build_system_prompt(
            "You are helpful.",
            "",
            dir.path(),
            &tools,
            &env,
            &[],
            &registry,
        );
        assert!(result.contains("**Read**"));
        assert!(result.contains("Read a file"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        // Write an agent JSON with a description
        std::fs::write(
            dir.path().join("scout.json"),
            r#"{"name":"scout","description":"Scouting agent.","system_prompt":"You scout."}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("scout"));
        assert!(result.contains("Scouting agent."));
        assert!(result.contains("Sub-Agents"));
    }

    #[test]
    fn test_build_system_prompt_skips_koda_agent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("koda.json"),
            r#"{"name":"koda","system_prompt":"main"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("scout.json"),
            r#"{"name":"scout","system_prompt":"scout"}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        // koda (the main agent) must not appear in the sub-agents listing.
        // Check the full result: the agent formatter produces "- **name**" (with desc)
        // or "- name" (without). Neither should match "koda".
        assert!(
            !result.contains("- **koda**") && !result.contains("\n- koda\n"),
            "koda should not appear as a sub-agent: {result}"
        );
        // scout has no description in this JSON, renders as "- scout"
        assert!(
            result.contains("scout"),
            "scout should appear in the sub-agents section: {result}"
        );
    }

    #[test]
    fn test_environment_section_present() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("## Environment"));
        assert!(result.contains("/test/project"));
        assert!(result.contains("test-model"));
        assert!(result.contains("test-os"));
    }

    #[test]
    fn test_instructions_included() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        // Spot-check key sections from instructions.md
        assert!(result.contains("## Doing Tasks"));
        assert!(result.contains("## Executing Actions"));
        assert!(result.contains("## Using Your Tools"));
        assert!(result.contains("## Output"));
    }

    #[test]
    fn test_commands_generated_from_registry() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let commands = &[("/help", "Show help"), ("/exit", "Quit")];
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, commands, &registry);
        assert!(result.contains("`/help`"));
        assert!(result.contains("Show help"));
        assert!(result.contains("`/exit`"));
        assert!(result.contains("Commands (user types these in the REPL)"));
    }

    #[test]
    fn test_no_commands_section_for_sub_agents() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(!result.contains("Commands (user types these in the REPL)"));
    }

    #[test]
    fn test_skills_section_empty_registry() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("## Skills"));
        assert!(result.contains("No skills are currently available"));
    }

    #[test]
    fn test_skills_section_lists_skills() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        registry.add_builtin(
            "code-review",
            "Senior code review",
            Some("Use when asked to review code or a PR."),
            "# Review\nDo it.",
        );
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("code-review"));
        assert!(result.contains("Senior code review"));
        assert!(result.contains("Use when asked to review code or a PR."));
        // Must include the blocking requirement instruction
        assert!(result.contains("MUST call `ActivateSkill` FIRST"));
    }

    #[test]
    fn test_skills_section_no_when_to_use() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        registry.add_builtin("plain", "Plain skill", None, "content");
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("**plain**"));
        assert!(result.contains("Plain skill"));
    }

    #[test]
    fn test_skills_section_shows_metadata() {
        use crate::skills::{Skill, SkillMeta, SkillSource};

        let dir = TempDir::new().unwrap();
        let env = test_env();
        let mut registry = SkillRegistry::default();
        // Inject a skill with all metadata fields populated
        registry.skills.insert(
            "scoped".to_string(),
            Skill {
                meta: SkillMeta {
                    name: "scoped".to_string(),
                    description: "Scoped skill".to_string(),
                    tags: vec![],
                    when_to_use: Some("Use for scoped work".to_string()),
                    allowed_tools: vec!["Read".to_string(), "Grep".to_string()],
                    user_invocable: false,
                    argument_hint: Some("<file_path>".to_string()),
                    source: SkillSource::BuiltIn,
                },
                content: "scoped content".to_string(),
            },
        );
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        assert!(result.contains("**scoped**"), "skill name");
        assert!(result.contains("Scoped skill"), "description");
        assert!(result.contains("Use for scoped work"), "when_to_use");
        assert!(result.contains("(Tools: Read, Grep)"), "allowed_tools");
        assert!(result.contains("`<file_path>`"), "argument_hint");
        assert!(result.contains("[model-only]"), "user_invocable=false");
    }

    #[test]
    fn test_agents_sorted_alphabetically() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("zebra.json"),
            r#"{"name":"zebra","system_prompt":"z"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("alpha.json"),
            r#"{"name":"alpha","system_prompt":"a"}"#,
        )
        .unwrap();
        let env = test_env();
        let registry = SkillRegistry::default();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[], &registry);
        let alpha_pos = result.find("alpha").unwrap();
        let zebra_pos = result.find("zebra").unwrap();
        assert!(alpha_pos < zebra_pos, "agents should be sorted A→Z");
    }
}
