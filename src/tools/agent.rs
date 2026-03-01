//! Sub-agent invocation and discovery tools.
//!
//! Exposes `InvokeAgent` and `ListAgents` as tools the LLM can call.
//! Actual sub-agent execution is handled by the event loop since it needs
//! access to config, DB, and the provider.

use crate::providers::ToolDefinition;
use serde_json::json;
use std::path::Path;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "InvokeAgent".to_string(),
            description: "Delegate a task to a specialized sub-agent. The sub-agent runs \
                independently with its own persona and tools, then returns its result."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Name of the sub-agent (must be one from ListAgents)"
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task to delegate to the sub-agent"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional session ID to continue a previous sub-agent conversation"
                    }
                },
                "required": ["agent_name", "prompt"]
            }),
        },
        ToolDefinition {
            name: "ListAgents".to_string(),
            description: "List all available sub-agents that can be invoked.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

/// Scan the agents/ directory and return a formatted list of available agents.
pub fn list_agents(project_root: &Path) -> String {
    let agents_dir = project_root.join("agents");
    let Ok(entries) = std::fs::read_dir(&agents_dir) else {
        return "No agents/ directory found.".to_string();
    };

    let mut agents: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let agent_name = name.strip_suffix(".json")?.to_string();

            // Skip the default agent — it's the main agent, not a sub-agent
            if agent_name == "default" {
                return None;
            }

            let content = std::fs::read_to_string(entry.path()).ok()?;
            let config: serde_json::Value = serde_json::from_str(&content).ok()?;

            // Extract a clean description from the first sentence of system_prompt
            let desc = config["system_prompt"]
                .as_str()
                .map(|s| extract_description(s))
                .unwrap_or_default();

            Some((agent_name, desc))
        })
        .collect();

    agents.sort_by(|a, b| a.0.cmp(&b.0));

    if agents.is_empty() {
        "No sub-agents configured.".to_string()
    } else {
        let lines: Vec<String> = agents
            .iter()
            .map(|(name, desc)| format!("  \x1b[36m{name}\x1b[0m \u{2014} {desc}"))
            .collect();
        lines.join("\n")
    }
}

/// Extract a clean one-line description from a system prompt.
/// Looks for "Your job is to ..." or falls back to the first sentence.
fn extract_description(prompt: &str) -> String {
    // Try to find "Your job is to ..." pattern
    if let Some(idx) = prompt.find("Your job is to ") {
        let rest = &prompt[idx + "Your job is to ".len()..];
        let end = rest.find('.').unwrap_or(rest.len().min(80));
        let desc: String = rest[..end].chars().take(80).collect();
        return capitalize_first(&desc);
    }

    // Try "You are a ..." pattern — extract the role
    if let Some(idx) = prompt.find("You are a ") {
        let rest = &prompt[idx + "You are a ".len()..];
        let end = rest.find('.').unwrap_or(rest.len().min(60));
        let role: String = rest[..end].chars().take(60).collect();
        return capitalize_first(&role);
    }

    // Fallback: first line, capped
    let first_line = prompt.lines().next().unwrap_or("");
    let capped: String = first_line.chars().take(60).collect();
    capped
}

/// Capitalize the first character of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_definitions_count() {
        let defs = definitions();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].name, "InvokeAgent");
        assert_eq!(defs[1].name, "ListAgents");
    }

    #[test]
    fn test_list_agents_empty_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("agents")).unwrap();
        let result = list_agents(dir.path());
        assert_eq!(result, "No sub-agents configured.");
    }

    #[test]
    fn test_list_agents_no_dir() {
        let dir = TempDir::new().unwrap();
        let result = list_agents(dir.path());
        assert!(result.contains("No agents/ directory"));
    }

    #[test]
    fn test_list_agents_with_agents() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.json"),
            r#"{"name":"reviewer","system_prompt":"You are a senior code reviewer. Your job is to find bugs."}"#,
        ).unwrap();
        let result = list_agents(dir.path());
        assert!(result.contains("reviewer"));
        assert!(result.contains("Find bugs"));
    }

    #[test]
    fn test_list_agents_excludes_default() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("default.json"),
            r#"{"name":"default","system_prompt":"You are the default agent."}"#,
        ).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.json"),
            r#"{"name":"reviewer","system_prompt":"You are a code reviewer. Your job is to review code."}"#,
        ).unwrap();
        let result = list_agents(dir.path());
        assert!(!result.contains("default"), "Should exclude default agent");
        assert!(result.contains("reviewer"));
    }

    #[test]
    fn test_list_agents_only_default_shows_empty() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("default.json"),
            r#"{"name":"default","system_prompt":"Main agent."}"#,
        ).unwrap();
        let result = list_agents(dir.path());
        assert_eq!(result, "No sub-agents configured.");
    }

    #[test]
    fn test_extract_description_job_pattern() {
        let desc = extract_description("You are a reviewer. Your job is to find bugs and improvements.");
        assert_eq!(desc, "Find bugs and improvements");
    }

    #[test]
    fn test_extract_description_role_pattern() {
        let desc = extract_description("You are a paranoid security auditor.");
        assert_eq!(desc, "Paranoid security auditor");
    }

    #[test]
    fn test_extract_description_fallback() {
        let desc = extract_description("Review all the code carefully.");
        assert_eq!(desc, "Review all the code carefully.");
    }
}