//! System prompt construction.
//!
//! Builds the system prompt from agent config, memory, and available tools.

use std::path::Path;

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
pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
    env: &EnvironmentInfo<'_>,
    commands: &[(&str, &str)],
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

    // Sub-agents
    let available_agents = list_available_agents(agents_dir);
    if !available_agents.is_empty() {
        prompt.push_str("\n\n## Available Sub-Agents\n\n");
        prompt.push_str(
            "Use InvokeAgent for autonomous multi-step workflows that create/modify \
             files and need iteration (test generation, releases). \
             Do NOT invent agent names that are not listed here.\n",
        );
        for name in &available_agents {
            prompt.push_str(&format!("- {name}\n"));
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
             Sub-agent results are NOT visible to the user \u{2014} always summarize key findings.\n",
        );
    } else {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    // Skills
    prompt.push_str(
        "\n## Skills\n\n\
         Expert instruction modules \u{2014} zero cost, instant activation via `ActivateSkill`.\n\
         Use ListSkills to see what\u{2019}s available. \
         Prefer skills over sub-agents for read-only analysis tasks.\n\
         Custom: `.koda/skills/<name>/SKILL.md` (project) or `~/.config/koda/skills/<name>/SKILL.md` (global).\n",
    );

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

/// Scan the agents/ directory and return available agent names.
fn list_available_agents(agents_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.strip_suffix(".json").map(|s| s.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let result = build_system_prompt("You are helpful.", "", dir.path(), &[], &env, &[]);
        assert!(result.starts_with("You are helpful."));
        assert!(result.contains("Doing Tasks"));
        assert!(result.contains("Koda Quick Reference"));
        assert!(!result.contains("Project Memory"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt(
            "You are helpful.",
            "This is a Rust project.",
            dir.path(),
            &[],
            &env,
            &[],
        );
        assert!(result.contains("Project Memory"));
        assert!(result.contains("Rust project"));
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let tools = vec![crate::providers::ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file. Returns contents.".to_string(),
            parameters: serde_json::json!({}),
        }];
        let result = build_system_prompt("You are helpful.", "", dir.path(), &tools, &env, &[]);
        assert!(result.contains("**Read**"));
        assert!(result.contains("Read a file"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("scout.json"), "{}").unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[]);
        assert!(result.contains("scout"));
        assert!(result.contains("Sub-Agents"));
    }

    #[test]
    fn test_environment_section_present() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[]);
        assert!(result.contains("## Environment"));
        assert!(result.contains("/test/project"));
        assert!(result.contains("test-model"));
        assert!(result.contains("test-os"));
    }

    #[test]
    fn test_instructions_included() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[]);
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
        let commands = &[("/help", "Show help"), ("/exit", "Quit")];
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, commands);
        assert!(result.contains("`/help`"));
        assert!(result.contains("Show help"));
        assert!(result.contains("`/exit`"));
        assert!(result.contains("Commands (user types these in the REPL)"));
    }

    #[test]
    fn test_no_commands_section_for_sub_agents() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env, &[]);
        assert!(!result.contains("Commands (user types these in the REPL)"));
    }
}
