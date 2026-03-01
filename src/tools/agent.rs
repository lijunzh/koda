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

    let agents: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let agent_name = name.strip_suffix(".json")?;

            // Try to read the system_prompt for a brief description
            let content = std::fs::read_to_string(entry.path()).ok()?;
            let config: serde_json::Value = serde_json::from_str(&content).ok()?;
            let desc = config["system_prompt"]
                .as_str()
                .map(|s| {
                    let trimmed: String = s.chars().take(80).collect();
                    if s.len() > 80 {
                        format!("{trimmed}...")
                    } else {
                        trimmed
                    }
                })
                .unwrap_or_default();

            Some(format!("- {agent_name}: {desc}"))
        })
        .collect();

    if agents.is_empty() {
        "No agents configured.".to_string()
    } else {
        format!("Available agents:\n{}", agents.join("\n"))
    }
}
