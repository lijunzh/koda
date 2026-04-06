//! Skill activation tools for the LLM.
//!
//! Provides `ActivateSkill` and `ListSkills` tools that let the LLM
//! inject expertise into its context by loading `SKILL.md` files.
//!
//! ## ActivateSkill
//!
//! - **`skill_name`** (required) — name of the skill to load
//! - Injects the skill's instructions into the conversation
//! - Zero LLM cost (prompt injection, no extra inference call)
//!
//! ## ListSkills
//!
//! - No parameters — returns all available skills with descriptions
//!
//! See [`crate::skills`] for skill discovery, file format, and built-in skills.

use crate::providers::ToolDefinition;
use crate::skills::SkillRegistry;
use serde_json::json;

/// Tool definitions for `ListSkills` and `ActivateSkill`.
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

/// List available skills, optionally filtered by `query`.
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

/// Load a skill's full SKILL.md content by name.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write a minimal valid SKILL.md to `<tmp>/.koda/skills/<skill_name>/SKILL.md`.
    fn write_project_skill(tmp: &TempDir, skill_name: &str, description: &str) {
        let dir = tmp.path().join(".koda").join("skills").join(skill_name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {skill_name}\ndescription: {description}\ntags: [test]\n---\n\nInstructions for {skill_name}."
        );
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    // ── definitions ──────────────────────────────────────────────────────

    #[test]
    fn test_definitions_returns_two_tools() {
        assert_eq!(definitions().len(), 2);
    }

    #[test]
    fn test_definition_names() {
        let names: Vec<String> = definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"ListSkills".to_string()));
        assert!(names.contains(&"ActivateSkill".to_string()));
    }

    #[test]
    fn test_activate_skill_requires_skill_name() {
        let d = definitions()
            .into_iter()
            .find(|d| d.name == "ActivateSkill")
            .unwrap();
        let required: Vec<&str> = d.parameters["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"skill_name"));
    }

    #[test]
    fn test_list_skills_has_no_required_params() {
        let d = definitions()
            .into_iter()
            .find(|d| d.name == "ListSkills")
            .unwrap();
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.is_empty());
    }

    // ── list_skills ────────────────────────────────────────────────────

    #[test]
    fn test_list_skills_empty_registry() {
        let registry = SkillRegistry::default();
        let result = list_skills(&registry, &json!({}));
        assert_eq!(result, "No skills available.");
    }

    #[test]
    fn test_list_skills_shows_skill_name() {
        let tmp = TempDir::new().unwrap();
        write_project_skill(&tmp, "my-skill", "Does something cool");
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({}));
        assert!(
            result.contains("my-skill"),
            "should list the project skill: {result}"
        );
    }

    #[test]
    fn test_list_skills_query_matches() {
        let tmp = TempDir::new().unwrap();
        write_project_skill(&tmp, "cool-skill", "Something unique to filter on");
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({"query": "unique to filter"}));
        assert!(result.contains("cool-skill"));
    }

    #[test]
    fn test_list_skills_query_no_match() {
        let tmp = TempDir::new().unwrap();
        write_project_skill(&tmp, "my-skill", "mundane description");
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({"query": "zzz-not-found-anywhere"} ));
        assert!(result.contains("No skills found matching"));
    }

    // ── activate_skill ─────────────────────────────────────────────────

    #[test]
    fn test_activate_skill_missing_param() {
        let registry = SkillRegistry::default();
        let result = activate_skill(&registry, &json!({}));
        assert_eq!(result, "Missing 'skill_name' parameter.");
    }

    #[test]
    fn test_activate_skill_unknown_name() {
        let registry = SkillRegistry::default();
        let result = activate_skill(&registry, &json!({"skill_name": "does-not-exist"}));
        assert!(result.contains("not found"));
    }

    #[test]
    fn test_activate_skill_known_returns_content() {
        let tmp = TempDir::new().unwrap();
        write_project_skill(&tmp, "alpha", "Alpha skill");
        let registry = SkillRegistry::discover(tmp.path());
        let result = activate_skill(&registry, &json!({"skill_name": "alpha"}));
        assert!(
            result.contains("activated"),
            "expected activation message: {result}"
        );
        assert!(result.contains("Instructions for alpha"));
    }
}
