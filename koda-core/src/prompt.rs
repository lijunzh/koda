//! System prompt construction.
//!
//! Builds the system prompt from agent config, memory, and available tools.

use crate::model_tier::ModelTier;
use std::path::Path;

pub fn build_system_prompt(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
) -> String {
    build_system_prompt_tiered(
        base_prompt,
        semantic_memory,
        agents_dir,
        tool_defs,
        ModelTier::Standard,
    )
}

/// Build system prompt with tier-aware verbosity.
pub fn build_system_prompt_tiered(
    base_prompt: &str,
    semantic_memory: &str,
    agents_dir: &Path,
    tool_defs: &[crate::providers::ToolDefinition],
    tier: ModelTier,
) -> String {
    // Tier-specific base prompt transformation
    let mut prompt = match tier {
        ModelTier::Strong => build_strong_persona(base_prompt),
        ModelTier::Lite => build_lite_persona(base_prompt),
        ModelTier::Standard => base_prompt.to_string(),
    };

    // Planning and self-review instructions (#156 P0)
    prompt.push_str(
        "\n\n## Planning\n\
         For complex tasks (>3 steps), outline your plan before executing. \
         Review feasibility before proceeding.\n\
         Before executing a multi-step plan, briefly verify each step is \
         feasible with the information you have.\n",
    );

    // Embed the capabilities reference — Strong tier skips it (can discover)
    if tier != ModelTier::Strong {
        prompt.push_str("\n\n");
        prompt.push_str(include_str!("capabilities.md"));
    } else {
        // Strong tier gets compact category hints instead
        prompt.push_str(&crate::tools::discover::category_hints());
    }

    // Auto-generate tool reference from definitions
    if !tool_defs.is_empty() {
        prompt.push_str("\n### Available Tools\n\n");
        for def in tool_defs {
            let desc = match tier {
                ModelTier::Strong => {
                    // First sentence only (concise)
                    def.description
                        .split('.')
                        .next()
                        .unwrap_or(&def.description)
                        .to_string()
                }
                ModelTier::Lite => {
                    // Full description for weak models
                    def.description.clone()
                }
                ModelTier::Standard => {
                    // First sentence
                    def.description
                        .split('.')
                        .next()
                        .unwrap_or(&def.description)
                        .to_string()
                }
            };
            prompt.push_str(&format!("- **{}**: {}\n", def.name, desc));
        }
    }

    let available_agents = list_available_agents(agents_dir);
    if !available_agents.is_empty() {
        match tier {
            ModelTier::Strong => {
                // Strong: just agent names, minimal instructions
                prompt.push_str("\n\n## Sub-Agents\n");
                for name in &available_agents {
                    prompt.push_str(&format!("- {name}\n"));
                }
            }
            _ => {
                // Standard/Lite: full instructions
                prompt.push_str("\n\n## Available Sub-Agents\n");
                prompt.push_str(
                    "Use InvokeAgent for autonomous multi-step workflows that create/modify \
                     files and need iteration (test generation, releases). \
                     Do NOT invent agent names that are not listed here.\n",
                );
                for name in &available_agents {
                    prompt.push_str(&format!("- {name}\n"));
                }
            }
        }
    } else if tier != ModelTier::Strong {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    // Skills section — skip for Strong (discoverable)
    if tier != ModelTier::Strong {
        prompt.push_str(
            "\n## Skills\n\
             Use ActivateSkill for analysis, review, conventions, and checklists. \
             Skills inject expert instructions into your context \u{2014} zero cost, instant. \
             Use ListSkills to see what\u{2019}s available. \
             Prefer skills over sub-agents for read-only analysis tasks.\n",
        );
    }

    if !semantic_memory.is_empty() {
        prompt.push_str(&format!(
            "\n## Project Memory\n\
             The following are learned facts about this project:\n\
             {semantic_memory}"
        ));
    }

    prompt
}

// ── Tier-specific persona builders ─────────────────────────

/// Strong tier: compress the base prompt to essentials only.
/// Strong models infer intent from minimal instructions.
fn build_strong_persona(base_prompt: &str) -> String {
    // Extract just the agent name/identity line (first line or first sentence)
    let identity = base_prompt
        .lines()
        .next()
        .unwrap_or("You are Koda, an AI coding agent.");

    format!(
        "{identity}\n\n\
         Principles: DRY, YAGNI, SOLID. Prefer tools over shell equivalents. \
         Explore → read → edit → verify → summarize. \
         Conventional commits. Never force push. Plan complex tasks before executing."
    )
}

/// Lite tier: expand the base prompt with explicit step-by-step guidance.
/// Weak models need hand-holding and concrete examples.
fn build_lite_persona(base_prompt: &str) -> String {
    let mut prompt = base_prompt.to_string();

    prompt.push_str(
        "\n\n## Step-by-Step Guide\n\n\
         When given a task, follow these steps IN ORDER:\n\n\
         1. **Understand**: Read the relevant files first. Use `List` to see the directory structure, \
            then `Read` to examine specific files. Use `Grep` to search for patterns.\n\
         2. **Plan**: Before making any changes, describe what you will do in 3-5 bullet points.\n\
         3. **Execute**: Make changes one file at a time using `Edit` (for modifications) or \
            `Write` (for new files). Keep each edit small and focused.\n\
         4. **Verify**: After making changes, run the project's test suite using `Bash`. \
            Check for compilation errors and test failures.\n\
         5. **Report**: Summarize what you changed and why.\n\n\
         ## Important Rules\n\n\
         - ALWAYS use `Read` to check a file's contents before editing it.\n\
         - NEVER guess at file contents or project structure — always verify first.\n\
         - Use `Edit` for modifying existing files. Use `Write` only for new files.\n\
         - If you're unsure, ask the user rather than guessing.\n\
         - Only make one change at a time. Do not try to do everything in one step.\n",
    );

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

    // ── Tier-specific tests ───────────────────────────────

    #[test]
    fn test_strong_prompt_is_compact() {
        let dir = TempDir::new().unwrap();
        let strong = build_system_prompt_tiered(
            "You are Koda 🐻, a reliable AI coding assistant.",
            "",
            dir.path(),
            &[],
            ModelTier::Strong,
        );
        let standard = build_system_prompt_tiered(
            "You are Koda 🐻, a reliable AI coding assistant.",
            "",
            dir.path(),
            &[],
            ModelTier::Standard,
        );
        // Strong should be significantly shorter
        assert!(
            strong.len() < standard.len(),
            "Strong ({}) should be shorter than Standard ({})",
            strong.len(),
            standard.len()
        );
        // Strong should NOT have capabilities reference
        assert!(!strong.contains("Koda Quick Reference"));
        // Strong should have category hints
        assert!(strong.contains("Extended Capabilities"));
        // Strong should have compact persona
        assert!(strong.contains("DRY, YAGNI, SOLID"));
    }

    #[test]
    fn test_lite_prompt_is_verbose() {
        let dir = TempDir::new().unwrap();
        let tools = vec![crate::providers::ToolDefinition {
            name: "Read".to_string(),
            description: "Read a file from disk. Returns the full content.".to_string(),
            parameters: serde_json::json!({}),
        }];
        let lite =
            build_system_prompt_tiered("You are Koda.", "", dir.path(), &tools, ModelTier::Lite);
        // Lite should have step-by-step guide
        assert!(lite.contains("Step-by-Step Guide"));
        assert!(lite.contains("ALWAYS use `Read`"));
        // Lite should have full tool descriptions (not just first sentence)
        assert!(lite.contains("Returns the full content"));
        // Lite should have capabilities reference
        assert!(lite.contains("Koda Quick Reference"));
    }

    #[test]
    fn test_standard_prompt_is_unchanged() {
        let dir = TempDir::new().unwrap();
        let standard =
            build_system_prompt_tiered("You are Koda.", "", dir.path(), &[], ModelTier::Standard);
        let default = build_system_prompt("You are Koda.", "", dir.path(), &[]);
        // Standard tier should be identical to the non-tiered version
        assert_eq!(standard, default);
    }

    #[test]
    fn test_strong_skips_skills_section() {
        let dir = TempDir::new().unwrap();
        let strong =
            build_system_prompt_tiered("You are Koda.", "", dir.path(), &[], ModelTier::Strong);
        // The verbose skills instruction block should be skipped for Strong
        assert!(!strong.contains("Prefer skills over sub-agents"));
    }

    #[test]
    fn test_strong_compact_agent_listing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("scout.json"), "{}").unwrap();
        let strong =
            build_system_prompt_tiered("You are Koda.", "", dir.path(), &[], ModelTier::Strong);
        assert!(strong.contains("scout"));
        // Should NOT have the verbose "Do NOT invent agent names" instruction
        assert!(!strong.contains("Do NOT invent agent names"));
    }
}
