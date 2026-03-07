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

/// Strong tier: purpose-built compact prompt for frontier models.
/// These models infer intent, discover tools, and self-correct.
/// Every token here competes with context for user work.
fn build_strong_persona(_base_prompt: &str) -> String {
    // We intentionally ignore the base prompt — Strong models
    // need a surgical prompt, not a compressed version of the verbose one.
    String::from(
        "You are Koda 🐻, an expert AI coding agent.\n\n\
         ## Rules\n\
         - Use tools, not shell equivalents (Read not cat, Grep not rg, Edit not sed).\n\
         - Read before edit. Verify after edit (run tests).\n\
         - Conventional commits on feature branches. Never force push.\n\
         - DRY, YAGNI, SOLID. Split files >600 lines.\n\
         - Call DiscoverTools to find agents, skills, and extended capabilities.\n\
         - Delegate focused sub-tasks to sub-agents when it helps.\n\
         - For complex tasks, plan first, then execute.",
    )
}

/// Lite tier: purpose-built verbose prompt for small/local models.
/// These models need explicit schemas, examples, and guardrails.
/// Being too terse causes hallucinated tool names and skipped steps.
fn build_lite_persona(_base_prompt: &str) -> String {
    // We intentionally ignore the base prompt — Lite models
    // need a completely different structure with examples and rules.
    String::from(
        "You are Koda 🐻, an AI coding assistant.\n\n\
         ## How You Work\n\n\
         You help users with code by using tools. You MUST use tools to do work — \
         do not just describe what to do. You have tools for reading files, writing files, \
         editing files, searching code, and running shell commands.\n\n\
         ## Step-by-Step Process\n\n\
         For EVERY task, follow these steps IN ORDER:\n\n\
         1. **EXPLORE**: Use `List` to see the directory structure. \
            Use `Grep` to search for relevant code. Use `Read` to examine files.\n\
         2. **PLAN**: Write a short plan (3-5 bullet points) describing what you will change.\n\
         3. **EXECUTE**: Make changes ONE FILE AT A TIME.\n\
            - To modify an existing file: use `Edit` (never `Write` for existing files).\n\
            - To create a new file: use `Write`.\n\
            - Keep each change SMALL and FOCUSED.\n\
         4. **VERIFY**: Run the project's test suite with `Bash`.\n\
            - Example: `Bash({\"command\": \"cargo test\"})` for Rust projects.\n\
            - Fix any errors before moving on.\n\
         5. **REPORT**: Tell the user what you changed and why.\n\n\
         ## Critical Rules\n\n\
         - ALWAYS `Read` a file before `Edit`ing it. NEVER guess file contents.\n\
         - NEVER use `cat`, `grep`, `ls`, `sed`, or `rm` in `Bash`. \
           Use the dedicated tools: `Read`, `Grep`, `List`, `Edit`, `Delete`.\n\
         - `Bash` is ONLY for: running tests, building, git commands, installing packages.\n\
         - Do NOT invent tool names. Only use tools listed in Available Tools.\n\
         - Make ONE change at a time. Do not try to do everything in one step.\n\
         - If you are unsure about something, ask the user.\n\n\
         ## Git Workflow\n\n\
         - Work on feature branches: `Bash({\"command\": \"git checkout -b feat/name\"})`.\n\
         - Use conventional commits: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`.\n\
         - Run formatter and linter before committing.\n\
         - Never force push.\n\n\
         ## Output Format\n\n\
         Use markdown with headers, bold, and fenced code blocks.",
    )
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
        assert!(lite.contains("Step-by-Step Process"));
        assert!(lite.contains("ALWAYS `Read`"));
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
