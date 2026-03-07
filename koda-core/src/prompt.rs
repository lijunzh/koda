//! System prompt construction.
//!
//! Builds the system prompt from agent config, memory, and available tools.

use std::path::Path;

pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
) -> String {
    let mut prompt = base_prompt.to_string();

    // Embed the capabilities reference (REPL features, not tools)
    prompt.push_str("\n\n");
    prompt.push_str(include_str!("capabilities.md"));

    // Auto-generate tool reference from definitions
    if !tool_defs.is_empty() {
        prompt.push_str("\n### Available Tools\n\n");
        for def in tool_defs {
            // First sentence of description only (keep it concise)
            let short_desc = def
                .description
                .split('.')
                .next()
                .unwrap_or(&def.description);
            prompt.push_str(&format!("- **{}**: {}\n", def.name, short_desc));
        }
    }

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

    prompt.push_str(
        "\n## Skills\n\
         Use ActivateSkill for analysis, review, conventions, and checklists. \
         Skills inject expert instructions into your context \u{2014} zero cost, instant. \
         Use ListSkills to see what\u{2019}s available. \
         Prefer skills over sub-agents for read-only analysis tasks.\n",
    );

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

// ── Utilities ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_build_system_prompt_no_agents_no_memory() {
        let dir = TempDir::new().unwrap();
        let result = build_system_prompt("You are helpful.", "", dir.path(), &[]);
        assert!(result.contains("You are helpful."));
        assert!(result.contains("No sub-agents are configured"));
        assert!(!result.contains("Project Memory"));
        // Capabilities reference is always embedded
        assert!(result.contains("Koda Quick Reference"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = TempDir::new().unwrap();
        let result =
            build_system_prompt("Base prompt.", "Uses Rust. Prefers tokio.", dir.path(), &[]);
        assert!(result.contains("Base prompt."));
        assert!(result.contains("Project Memory"));
        assert!(result.contains("Uses Rust"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("reviewer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("planner.json"), "{}").unwrap();

        let result = build_system_prompt("Base.", "", dir.path(), &[]);
        assert!(result.contains("Available Sub-Agents"));
        assert!(result.contains("reviewer"));
        assert!(result.contains("planner"));
        assert!(!result.contains("No sub-agents"));
    }

    #[test]
    fn test_build_system_prompt_ignores_non_json() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("README.md"), "docs").unwrap();
        std::fs::write(dir.path().join("agent.json"), "{}").unwrap();

        let result = build_system_prompt("Base.", "", dir.path(), &[]);
        assert!(result.contains("agent"));
        // README.md should not appear as an agent
        assert!(!result.contains("README"));
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let dir = TempDir::new().unwrap();
        let tools = vec![crate::providers::ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file. Returns the content.".to_string(),
            parameters: serde_json::json!({}),
        }];
        let result = build_system_prompt("Base.", "", dir.path(), &tools);
        assert!(result.contains("Available Tools"));
        assert!(result.contains("**Read**"));
        assert!(result.contains("Read a file"));
        // Only first sentence
        assert!(!result.contains("Returns the content"));
    }
}
