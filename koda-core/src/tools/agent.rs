//! Sub-agent invocation and discovery tools.
//!
//! Exposes `InvokeAgent` and `ListAgents` as tools the LLM can call.
//! Actual sub-agent execution is handled by the event loop since it needs
//! access to config, DB, and the provider.

use crate::providers::ToolDefinition;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
            description: "List available sub-agents. Use detail=true to see system prompts."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "detail": {
                        "type": "boolean",
                        "description": "Show full system prompts"
                    }
                }
            }),
        },
    ]
}

/// Agent info from discovery: name, description, source, and optionally the full prompt.
pub struct AgentInfo {
    /// Agent name (used in `InvokeAgent` tool calls).
    pub name: String,
    /// One-line description shown in `ListAgents` output.
    pub description: String,
    /// Discovery source: `"built-in"`, `"user"`, or `"project"`.
    pub source: &'static str,
    /// Full system prompt content.
    pub system_prompt: String,
}

/// Discover all agents from all sources, with project > user > built-in priority.
pub fn discover_all_agents(project_root: &Path) -> Vec<AgentInfo> {
    let mut agents: HashMap<String, AgentInfo> = HashMap::new();

    // 1. Built-in agents (lowest priority)
    for (name, config) in crate::config::KodaConfig::builtin_agents() {
        if name == "default" {
            continue;
        }
        agents.insert(
            name.clone(),
            AgentInfo {
                name,
                description: extract_description(&config.system_prompt),
                source: "built-in",
                system_prompt: config.system_prompt,
            },
        );
    }

    // 2. User agents (~/.config/koda/agents/) — overrides built-ins
    if let Ok(user_dir) = user_agents_dir() {
        load_agents_from_dir(&user_dir, "user", &mut agents);
    }

    // 3. Project agents (<project>/agents/) — highest priority
    let project_dir = project_root.join("agents");
    load_agents_from_dir(&project_dir, "project", &mut agents);

    let mut result: Vec<AgentInfo> = agents.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

/// Load agents from a directory into the map (later calls override earlier).
fn load_agents_from_dir(dir: &Path, source: &'static str, agents: &mut HashMap<String, AgentInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(agent_name) = name.strip_suffix(".json") else {
            continue;
        };
        if agent_name == "default" {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let prompt = config["system_prompt"].as_str().unwrap_or("").to_string();
        agents.insert(
            agent_name.to_string(),
            AgentInfo {
                name: agent_name.to_string(),
                description: extract_description(&prompt),
                source,
                system_prompt: prompt,
            },
        );
    }
}

/// Return the user-level agents directory path.
fn user_agents_dir() -> Result<PathBuf, std::env::VarError> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("koda")
        .join("agents"))
}

/// Return agent list data for display (used by /agent command and ListAgents tool).
///
/// Returns a list of `(name, description, source)` tuples.
/// The client is responsible for formatting/coloring.
pub fn list_agents(project_root: &Path) -> Vec<(String, String, String)> {
    discover_all_agents(project_root)
        .into_iter()
        .map(|a| {
            (
                a.name.to_string(),
                a.description.to_string(),
                a.source.to_string(),
            )
        })
        .collect()
}

/// Format detailed agent list (for ListAgents with detail=true).
pub fn list_agents_detail(project_root: &Path) -> String {
    let agents = discover_all_agents(project_root);

    if agents.is_empty() {
        return "No sub-agents configured.".to_string();
    }

    let mut output = String::new();
    for a in &agents {
        output.push_str(&format!("## {} [{}]\n", a.name, a.source));
        // Show first 500 chars of prompt as template reference
        let preview: String = a.system_prompt.chars().take(500).collect();
        output.push_str(&preview);
        if a.system_prompt.len() > 500 {
            output.push_str("\n[...truncated]");
        }
        output.push_str("\n\n");
    }
    output
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
    fn test_list_agents_no_builtins() {
        let dir = TempDir::new().unwrap();
        let result = list_agents(dir.path());
        // No built-in sub-agents after purge (#329)
        // (user-level agents from ~/.config/koda/agents/ may still appear)
        let builtins: Vec<_> = result
            .iter()
            .filter(|(_, _, src)| src == "built-in")
            .collect();
        assert!(
            builtins.is_empty(),
            "No built-in sub-agents after purge (#329), got: {builtins:?}"
        );
        // Default is always excluded from listing
        let names: Vec<&str> = result.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(!names.contains(&"default"), "Should exclude default agent");
    }

    #[test]
    fn test_list_agents_project_overrides_builtin() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.json"),
            r#"{"name":"reviewer","system_prompt":"You are a custom project reviewer. Your job is to do project-specific reviews."}"#,
        ).unwrap();
        let result = list_agents(dir.path());
        let reviewer = result.iter().find(|(n, _, _)| n == "reviewer");
        assert!(reviewer.is_some());
        assert_eq!(
            reviewer.unwrap().2,
            "project",
            "Project agent should be tagged"
        );
    }

    #[test]
    fn test_discover_all_agents_no_builtins() {
        let dir = TempDir::new().unwrap();
        let agents = discover_all_agents(dir.path());
        let builtins: Vec<_> = agents.iter().filter(|a| a.source == "built-in").collect();
        assert_eq!(
            builtins.len(),
            0,
            "No built-in sub-agents after #329 purge, got {}",
            builtins.len()
        );
    }

    #[test]
    fn test_list_agents_detail_empty_when_no_builtins() {
        let dir = TempDir::new().unwrap();
        let result = list_agents_detail(dir.path());
        // No built-in sub-agents after #329 purge
        assert!(!result.contains("[built-in]"));
    }

    #[test]
    fn test_extract_description_job_pattern() {
        let desc =
            extract_description("You are a reviewer. Your job is to find bugs and improvements.");
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
