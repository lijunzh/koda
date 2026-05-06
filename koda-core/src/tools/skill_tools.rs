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
            description: "Activate a skill to load its expert instructions into context. \
                If the user's request matches a skill listed in the ## Skills section of \
                the system prompt, you MUST call this tool FIRST — before writing any \
                response. Do not answer from training data when a skill covers the topic."
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
        let hint = meta
            .argument_hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let tools_note = if meta.allowed_tools.is_empty() {
            String::new()
        } else {
            format!(" (Tools: {})", meta.allowed_tools.join(", "))
        };
        let visibility = if !meta.user_invocable {
            " [model-only]"
        } else {
            ""
        };
        out.push_str(&format!(
            "  \u{1f4da} {}{} \u{2014} {}{}{}{visibility}\n",
            meta.name, hint, meta.description, tags, tools_note
        ));
        if let Some(wtu) = &meta.when_to_use {
            out.push_str(&format!("    When to use: {wtu}\n"));
        }
    }
    out.push_str(&format!(
        "\n{} skill(s). Use ActivateSkill to load one.",
        skills.len()
    ));
    out
}

/// Load a skill's full SKILL.md content by name.
///
/// Returns the skill content along with metadata about scoped tools
/// (if `allowed_tools` is set in the skill's frontmatter).
pub fn activate_skill(registry: &SkillRegistry, args: &serde_json::Value) -> String {
    let name = match args.get("skill_name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "Missing 'skill_name' parameter.".to_string(),
    };

    match registry.get(name) {
        Some(skill) => {
            let mut result = format!(
                "Skill '{name}' activated. Follow these instructions:\n\n{}",
                skill.content
            );
            if !skill.meta.allowed_tools.is_empty() {
                result.push_str(&format!(
                    "\n\n[Skill scope: only use these tools: {}]",
                    skill.meta.allowed_tools.join(", ")
                ));
            }
            result
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

// =============================================================
// Tool trait implementations (#1265 item 5, PR-8/N).
//
// Both tools are read-only; both read the `SkillRegistry` off the
// context (added in PR-8). Pre-#1265 they were thin wrappers in
// the dispatch match arm; now they're a proper pair of structs.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `ListSkills` — enumerate available skills (built-in + user + project).
pub struct ListSkillsTool;

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &'static str {
        "ListSkills"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "ListSkills")
            .expect("skill_tools::definitions() must contain ListSkills")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        ToolResult {
            output: list_skills(ctx.skill_registry, args),
            success: true,
            full_output: None,
        }
    }
}

/// `ActivateSkill` — load a skill's full SKILL.md content.
pub struct ActivateSkillTool;

#[async_trait]
impl Tool for ActivateSkillTool {
    fn name(&self) -> &'static str {
        "ActivateSkill"
    }
    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .find(|d| d.name == "ActivateSkill")
            .expect("skill_tools::definitions() must contain ActivateSkill")
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        ToolResult {
            output: activate_skill(ctx.skill_registry, args),
            success: true,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Write a SKILL.md with optional `when_to_use` frontmatter.
    fn write_project_skill(tmp: &TempDir, skill_name: &str, description: &str) {
        let dir = tmp.path().join(".koda").join("skills").join(skill_name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {skill_name}\ndescription: {description}\ntags: [test]\n---\n\nInstructions for {skill_name}."
        );
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    fn write_project_skill_with_when(tmp: &TempDir, skill_name: &str, when_to_use: &str) {
        let dir = tmp.path().join(".koda").join("skills").join(skill_name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {skill_name}\ndescription: A skill with guidance.\ntags: []\nwhen_to_use: {when_to_use}\n---\n\nInstructions."
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

    #[test]
    fn test_list_skills_shows_when_to_use() {
        let tmp = TempDir::new().unwrap();
        write_project_skill_with_when(
            &tmp,
            "guided-skill",
            "Use when the user asks to do the thing.",
        );
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({}));
        assert!(
            result.contains("When to use: Use when the user asks to do the thing."),
            "should surface when_to_use in listing: {result}"
        );
    }

    #[test]
    fn test_list_skills_omits_when_to_use_line_if_absent() {
        // Use a bare registry (no built-ins) so we control exactly what's in it.
        let mut registry = SkillRegistry::default();
        registry.add_builtin("plain-skill", "no guidance", None, "content");
        let result = list_skills(&registry, &json!({}));
        assert!(
            !result.contains("When to use:"),
            "should not emit 'When to use:' when field is absent: {result}"
        );
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

    #[test]
    fn test_activate_skill_with_allowed_tools() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".koda").join("skills").join("scoped");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: scoped\ndescription: Scoped skill\ntags: []\nallowed_tools: [Read, Grep]\n---\n\nDo stuff.",
        )
        .unwrap();
        let registry = SkillRegistry::discover(tmp.path());
        let result = activate_skill(&registry, &json!({"skill_name": "scoped"}));
        assert!(result.contains("activated"), "should activate: {result}");
        assert!(result.contains("Do stuff."));
        assert!(
            result.contains("[Skill scope: only use these tools: Read, Grep]"),
            "should include tool scope: {result}"
        );
    }

    #[test]
    fn test_list_skills_shows_allowed_tools() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".koda").join("skills").join("scoped");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: scoped\ndescription: Scoped\ntags: []\nallowed_tools: [Read, Grep]\n---\n\ncontent",
        )
        .unwrap();
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({}));
        assert!(
            result.contains("(Tools: Read, Grep)"),
            "should show allowed tools: {result}"
        );
    }

    #[test]
    fn test_list_skills_shows_model_only_tag() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".koda").join("skills").join("hidden");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: hidden\ndescription: Hidden\ntags: []\nuser_invocable: false\n---\n\ncontent",
        )
        .unwrap();
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({}));
        assert!(
            result.contains("[model-only]"),
            "should show model-only tag: {result}"
        );
    }

    #[test]
    fn test_list_skills_shows_argument_hint() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".koda").join("skills").join("pdf");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: pdf\ndescription: Generate PDF\ntags: []\nargument_hint: <file_path>\n---\n\ncontent",
        )
        .unwrap();
        let registry = SkillRegistry::discover(tmp.path());
        let result = list_skills(&registry, &json!({}));
        assert!(
            result.contains("pdf <file_path>"),
            "should show argument hint: {result}"
        );
    }
}
