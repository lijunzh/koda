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
pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
    env: &EnvironmentInfo<'_>,
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

    // Embed the capabilities reference
    prompt.push('\n');
    prompt.push_str(include_str!("capabilities.md"));

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
        prompt.push_str("\n\n## Available Sub-Agents\n");
        prompt.push_str(
            "Use InvokeAgent for autonomous multi-step workflows that create/modify \
             files and need iteration (test generation, releases). \
             Do NOT invent agent names that are not listed here.\n",
        );
        for name in &available_agents {
            prompt.push_str(&format!("- {name}\n"));
        }
    } else {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    // Skills
    prompt.push_str(
        "\n## Skills\n\
         Use ActivateSkill for analysis, review, conventions, and checklists. \
         Skills inject expert instructions into your context \u{2014} zero cost, instant. \
         Use ListSkills to see what\u{2019}s available. \
         Prefer skills over sub-agents for read-only analysis tasks.\n",
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
        let result = build_system_prompt("You are helpful.", "", dir.path(), &[], &env);
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
        let result = build_system_prompt("You are helpful.", "", dir.path(), &tools, &env);
        assert!(result.contains("**Read**"));
        assert!(result.contains("Read a file"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("scout.json"), "{}").unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env);
        assert!(result.contains("scout"));
        assert!(result.contains("Sub-Agents"));
    }

    #[test]
    fn test_environment_section_present() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env);
        assert!(result.contains("## Environment"));
        assert!(result.contains("/test/project"));
        assert!(result.contains("test-model"));
        assert!(result.contains("test-os"));
    }

    #[test]
    fn test_instructions_included() {
        let dir = TempDir::new().unwrap();
        let env = test_env();
        let result = build_system_prompt("Base.", "", dir.path(), &[], &env);
        // Spot-check key sections from instructions.md
        assert!(result.contains("## Doing Tasks"));
        assert!(result.contains("## Executing Actions"));
        assert!(result.contains("## Using Your Tools"));
        assert!(result.contains("## Output"));
    }
}
