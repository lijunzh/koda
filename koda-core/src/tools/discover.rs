//! DiscoverTools — on-demand tool schema injection.
//!
//! For Strong-tier models, only core tools + DiscoverTools are loaded upfront.
//! The model calls `DiscoverTools({ category: "agents" })` to get the full
//! schemas for a category on demand.

use crate::providers::ToolDefinition;
use serde_json::json;

/// Tool categories for on-demand discovery.
const CATEGORIES: &[(&str, &str)] = &[
    (
        "agents",
        "Sub-agent management: InvokeAgent, ListAgents, CreateAgent",
    ),
    (
        "skills",
        "Expert skill activation: ListSkills, ActivateSkill",
    ),
    ("web", "Web content fetching: WebFetch"),
    ("memory", "Persistent memory: MemoryRead, MemoryWrite"),
    ("ast", "Code structure analysis: AstAnalysis"),
    (
        "email",
        "Email management: EmailRead, EmailSend, EmailSearch",
    ),
];

/// The DiscoverTools tool definition (lightweight, ~50 tokens).
pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "DiscoverTools".to_string(),
        description: "Discover additional tool capabilities by category. \
            Returns full tool schemas for the requested category."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "enum": ["agents", "skills", "web", "memory", "ast", "email", "all"],
                    "description": "Category of tools to discover"
                }
            },
            "required": ["category"]
        }),
    }
}

/// Core tool names — always loaded regardless of tier.
pub const CORE_TOOLS: &[&str] = &[
    "Read", "Write", "Edit", "Delete", "List", "Grep", "Glob", "Bash",
];

/// Tool names by category.
fn tools_in_category(category: &str) -> &'static [&'static str] {
    match category {
        "agents" => &["InvokeAgent", "ListAgents", "CreateAgent"],
        "skills" => &["ListSkills", "ActivateSkill"],
        "web" => &["WebFetch"],
        "memory" => &["MemoryRead", "MemoryWrite"],
        "ast" => &["AstAnalysis"],
        "email" => &["EmailRead", "EmailSend", "EmailSearch"],
        _ => &[],
    }
}

/// Execute the DiscoverTools tool. Returns JSON schemas as a string.
pub fn discover(all_defs: &[ToolDefinition], args: &serde_json::Value) -> String {
    let category = args["category"].as_str().unwrap_or("all");

    if category == "all" {
        // Return everything that isn't a core tool or DiscoverTools itself
        let extras: Vec<&ToolDefinition> = all_defs
            .iter()
            .filter(|d| !CORE_TOOLS.contains(&d.name.as_str()) && d.name != "DiscoverTools")
            .collect();
        return format_tools(&extras);
    }

    let target_names = tools_in_category(category);
    if target_names.is_empty() {
        return format!(
            "Unknown category: '{category}'. Available: {}",
            CATEGORIES
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let matched: Vec<&ToolDefinition> = all_defs
        .iter()
        .filter(|d| target_names.contains(&d.name.as_str()))
        .collect();

    if matched.is_empty() {
        format!("No tools found for category '{category}'. They may not be installed.")
    } else {
        format_tools(&matched)
    }
}

/// Format a list of tool definitions as readable output.
fn format_tools(tools: &[&ToolDefinition]) -> String {
    let mut out = String::new();
    for def in tools {
        out.push_str(&format!("### {}\n", def.name));
        out.push_str(&format!("{}\n", def.description));
        out.push_str(&format!(
            "Parameters: {}\n\n",
            serde_json::to_string_pretty(&def.parameters).unwrap_or_default()
        ));
    }
    out
}

/// Generate category hints for the system prompt (Strong tier).
pub fn category_hints() -> String {
    let mut hint = String::from(
        "\n## Extended Capabilities\n\n\
         You have additional capabilities available on demand.\n\
         Call DiscoverTools with a category to unlock them:\n\n",
    );
    for (name, desc) in CATEGORIES {
        hint.push_str(&format!("- **{name}** \u{2014} {desc}\n"));
    }
    hint.push_str("\nOnly discover what you need for the current task.\n");
    hint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_definition_has_category_enum() {
        let def = definition();
        assert_eq!(def.name, "DiscoverTools");
        let cat = &def.parameters["properties"]["category"];
        assert!(cat["enum"].is_array());
    }

    #[test]
    fn test_discover_all() {
        let defs = vec![
            ToolDefinition {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: json!({}),
            },
            ToolDefinition {
                name: "ListAgents".into(),
                description: "List agents".into(),
                parameters: json!({}),
            },
        ];
        let result = discover(&defs, &json!({ "category": "all" }));
        assert!(result.contains("ListAgents"));
        assert!(!result.contains("### Read")); // Read is core, excluded
    }

    #[test]
    fn test_discover_agents() {
        let defs = vec![
            ToolDefinition {
                name: "InvokeAgent".into(),
                description: "Invoke a sub-agent".into(),
                parameters: json!({}),
            },
            ToolDefinition {
                name: "Read".into(),
                description: "Read a file".into(),
                parameters: json!({}),
            },
        ];
        let result = discover(&defs, &json!({ "category": "agents" }));
        assert!(result.contains("InvokeAgent"));
        assert!(!result.contains("### Read"));
    }

    #[test]
    fn test_discover_unknown_category() {
        let result = discover(&[], &json!({ "category": "bogus" }));
        assert!(result.contains("Unknown category"));
    }

    #[test]
    fn test_category_hints() {
        let hints = category_hints();
        assert!(hints.contains("agents"));
        assert!(hints.contains("skills"));
        assert!(hints.contains("DiscoverTools"));
    }

    #[test]
    fn test_core_tools_list() {
        assert!(CORE_TOOLS.contains(&"Read"));
        assert!(CORE_TOOLS.contains(&"Bash"));
        assert!(!CORE_TOOLS.contains(&"InvokeAgent"));
    }
}
