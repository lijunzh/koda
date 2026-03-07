//! Skill activation tools for the LLM.
//!
//! Provides `ActivateSkill` and `ListSkills` tools that let the LLM
//! inject expertise into its context by loading SKILL.md files.

use crate::providers::ToolDefinition;
use crate::skills::SkillRegistry;
use serde_json::json;

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "ListSkills".to_string(),
            description: "List available skills (expertise modules for reviews, audits, etc.)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional search term to filter skills by name/description/tags"
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "ActivateSkill".to_string(),
            description: "Activate a skill for expert instructions. Follow the returned guidance."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "description": "Name of the skill to activate (from ListSkills)"
                    }
                },
                "required": ["skill_name"]
            }),
        },
    ]
}

pub fn list_skills(registry: &SkillRegistry, args: &serde_json::Value) -> String {
    let query = args.get("query").and_then(|v| v.as_str());

    let skills = match query {
        Some(q) if !q.is_empty() => registry.search(q),
        _ => registry.list(),
    };

    if skills.is_empty() {
        return match query {
            Some(q) => format!("No skills found matching '{q}'."),
            None => "No skills available.".to_string(),
        };
    }

    let mut out = String::from("Available skills:\n\n");
    for meta in &skills {
        let tags = if meta.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", meta.tags.join(", "))
        };
        out.push_str(&format!(
            "  \u{1f4da} {} \u{2014} {}{}\n",
            meta.name, meta.description, tags
        ));
    }
    out.push_str(&format!(
        "\n{} skill(s). Use ActivateSkill to load one.",
        skills.len()
    ));
    out
}

pub fn activate_skill(registry: &SkillRegistry, args: &serde_json::Value) -> String {
    let name = match args.get("skill_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "Missing 'skill_name' parameter.".to_string(),
    };

    match registry.activate(name) {
        Some(content) => {
            format!("Skill '{name}' activated. Follow these instructions:\n\n{content}")
        }
        None => {
            let available: Vec<String> = registry.list().iter().map(|m| m.name.clone()).collect();
            format!(
                "Skill '{name}' not found. Available: {}",
                available.join(", ")
            )
        }
    }
}
