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
        ToolDefinition {
            name: "CreateAgent".to_string(),
            description: "Create a new sub-agent for recurring specialized tasks. \
                Only for tasks that need a dedicated persona — not for one-off work."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Agent name (lowercase, no spaces). Used as the filename: agents/<name>.json"
                    },
                    "description": {
                        "type": "string",
                        "description": "One-line description of what this agent does"
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "Full system prompt for the agent"
                    },
                    "allowed_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tools this agent can use. Empty [] = all tools."
                    }
                },
                "required": ["name", "system_prompt"]
            }),
        },
    ]
}

/// Create a new sub-agent, validating the request first.
pub fn create_agent(project_root: &Path, args: &serde_json::Value) -> String {
    let Some(name) = args["name"].as_str() else {
        return "Error: 'name' is required.".to_string();
    };
    let Some(system_prompt) = args["system_prompt"].as_str() else {
        return "Error: 'system_prompt' is required.".to_string();
    };

    // Validate name
    if name.is_empty() || name.contains(' ') || name.contains('/') {
        return "Error: agent name must be lowercase with no spaces or slashes.".to_string();
    }
    if name == "default" {
        return "Error: cannot overwrite the default agent.".to_string();
    }

    // Check if agent already exists in any source (built-in, user, project)
    let all_agents = discover_all_agents(project_root);
    if let Some(existing) = all_agents.iter().find(|a| a.name == name) {
        return format!(
            "Error: agent '{}' already exists [{}]. Use Edit to modify it, or choose a different name.",
            name, existing.source
        );
    }

    // Validate system prompt has reasonable content
    if system_prompt.len() < 50 {
        return "Error: system_prompt is too short. Include identity, process, and output format."
            .to_string();
    }

    // Build the agent config
    let allowed_tools = args["allowed_tools"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let config = json!({
        "name": name,
        "system_prompt": system_prompt,
        "allowed_tools": allowed_tools,
        "model": null,
        "base_url": null
    });

    // Write to user config dir (~/.config/koda/agents/) so it's portable
    let Ok(agents_dir) = user_agents_dir() else {
        return "Error: could not determine user config directory.".to_string();
    };
    if let Err(e) = std::fs::create_dir_all(&agents_dir) {
        return format!("Error creating agents directory: {e}");
    }
    let agent_path = agents_dir.join(format!("{name}.json"));

    // Write the agent file
    match serde_json::to_string_pretty(&config) {
        Ok(json_str) => match std::fs::write(&agent_path, json_str) {
            Ok(()) => format!(
                "Created agent '{name}' at {}.\nUse /agent to see it, or ask me to invoke it.",
                agent_path.display()
            ),
            Err(e) => format!("Error writing agent file: {e}"),
        },
        Err(e) => format!("Error serializing agent config: {e}"),
    }
}

/// Agent info from discovery: name, description, source, and optionally the full prompt.
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    pub source: &'static str, // "built-in", "user", or "project"
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

/// Format detailed agent list (for ListAgents with detail=true, used by CreateAgent workflow).
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
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].name, "InvokeAgent");
        assert_eq!(defs[1].name, "ListAgents");
        assert_eq!(defs[2].name, "CreateAgent");
    }

    #[test]
    fn test_list_agents_includes_builtins() {
        let dir = TempDir::new().unwrap();
        let result = list_agents(dir.path());
        let names: Vec<&str> = result.iter().map(|(n, _, _)| n.as_str()).collect();
        // reviewer and security are now skills, not sub-agents
        assert!(
            names.contains(&"testgen"),
            "Should include built-in testgen"
        );
        assert!(
            names.contains(&"releaser"),
            "Should include built-in releaser"
        );
    }

    #[test]
    fn test_list_agents_excludes_default() {
        let dir = TempDir::new().unwrap();
        let result = list_agents(dir.path());
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
    fn test_discover_all_agents_has_builtins() {
        let dir = TempDir::new().unwrap();
        let agents = discover_all_agents(dir.path());
        // Should have at least the 4 built-in agents (excluding default)
        let builtins: Vec<_> = agents.iter().filter(|a| a.source == "built-in").collect();
        assert_eq!(
            builtins.len(),
            2,
            "Expected 2 built-in agents (testgen, releaser), got {}",
            builtins.len()
        );
        let names: Vec<_> = builtins.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"testgen"));
        assert!(names.contains(&"releaser"));
    }

    #[test]
    fn test_list_agents_detail_shows_prompts() {
        let dir = TempDir::new().unwrap();
        let result = list_agents_detail(dir.path());
        assert!(result.contains("## testgen [built-in]"));
        assert!(result.contains("## releaser [built-in]"));
        assert!(result.contains("You are a QA engineer"));
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

    #[test]
    fn test_create_agent_success() {
        // CreateAgent writes to ~/.config/koda/agents/, so we just verify
        // the output message (not the file) to avoid polluting user config
        let dir = TempDir::new().unwrap();
        let args = json!({
            "name": "test_temp_agent_xyz",
            "system_prompt": "You are a helpful agent. Your job is to do specialized things for the project with care and precision.",
            "allowed_tools": ["Read", "List"]
        });
        let result = create_agent(dir.path(), &args);
        assert!(
            result.contains("Created agent") || result.contains("already exists"),
            "Got: {result}"
        );
        // Clean up if created
        if result.contains("Created agent")
            && let Ok(user_dir) = user_agents_dir()
        {
            let _ = std::fs::remove_file(user_dir.join("test_temp_agent_xyz.json"));
        }
    }

    #[test]
    fn test_create_agent_rejects_default() {
        let dir = TempDir::new().unwrap();
        let args = json!({"name": "default", "system_prompt": "x".repeat(60)});
        let result = create_agent(dir.path(), &args);
        assert!(result.contains("cannot overwrite the default"));
    }

    #[test]
    fn test_create_agent_rejects_existing_builtin() {
        let dir = TempDir::new().unwrap();
        let args = json!({"name": "testgen", "system_prompt": "x".repeat(60)});
        let result = create_agent(dir.path(), &args);
        assert!(
            result.contains("already exists"),
            "Should reject duplicate of built-in: {result}"
        );
        assert!(
            result.contains("built-in"),
            "Should mention source: {result}"
        );
    }

    #[test]
    fn test_create_agent_rejects_existing_disk() {
        let dir = TempDir::new().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom.json"),
            r#"{"name":"custom","system_prompt":"x"}"#,
        )
        .unwrap();
        let args = json!({"name": "custom", "system_prompt": "x".repeat(60)});
        let result = create_agent(dir.path(), &args);
        assert!(
            result.contains("already exists"),
            "Should reject duplicate: {result}"
        );
    }

    #[test]
    fn test_create_agent_rejects_short_prompt() {
        let dir = TempDir::new().unwrap();
        let args = json!({"name": "bad", "system_prompt": "Too short."});
        let result = create_agent(dir.path(), &args);
        assert!(result.contains("too short"));
    }

    #[test]
    fn test_create_agent_rejects_bad_name() {
        let dir = TempDir::new().unwrap();
        let args = json!({"name": "bad name", "system_prompt": "x".repeat(60)});
        let result = create_agent(dir.path(), &args);
        assert!(result.contains("no spaces"));
    }
}
