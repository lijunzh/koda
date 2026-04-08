//! Skill-scoped tool filtering.
//!
//! When a skill with `allowed_tools` is activated, only those tools
//! (plus meta-tools like `ActivateSkill`, `ListSkills`, `ListAgents`,
//! `InvokeAgent`) are sent to the LLM. This is the "hard enforcement"
//! counterpart to the prompt hint in `activate_skill()`.
//!
//! ## How it works
//!
//! 1. The inference loop creates a `SkillToolScope` (initially empty).
//! 2. After each tool dispatch round, if an `ActivateSkill` call was made,
//!    the loop calls `update_from_tool_calls()` with the tool call names
//!    and args.
//! 3. `SkillToolScope` inspects the skill registry to check if the
//!    activated skill has `allowed_tools`.
//! 4. On the next iteration, `filter_tool_defs()` returns only the
//!    in-scope tools.
//!
//! ## Meta-tools
//!
//! These tools are always available regardless of scope, so the model
//! can switch skills or delegate:
//!
//! - `ActivateSkill`, `ListSkills`
//! - `ListAgents`, `InvokeAgent`
//! - `AskUser`
//!
//! ## Lifecycle
//!
//! - Activating a skill with `allowed_tools` → scope is set
//! - Activating a skill without `allowed_tools` → scope is cleared
//! - No `ActivateSkill` call → scope unchanged

use crate::providers::ToolDefinition;
use crate::skills::SkillRegistry;

/// Tools that are always available regardless of skill scope.
///
/// These let the model switch skills, delegate, or ask the user for help
/// even when scoped to a restricted tool set.
const META_TOOLS: &[&str] = &[
    "ActivateSkill",
    "ListSkills",
    "ListAgents",
    "InvokeAgent",
    "AskUser",
];

/// Tracks the active skill's tool scope during an inference loop.
///
/// Created once per `inference_loop` invocation. Updated after each
/// tool dispatch round.
#[derive(Debug, Default)]
pub struct SkillToolScope {
    /// Tool names allowed by the active skill, or `None` for unrestricted.
    allowed: Option<Vec<String>>,
}

impl SkillToolScope {
    /// Create a new unrestricted scope.
    pub fn new() -> Self {
        Self { allowed: None }
    }

    /// Check tool calls for `ActivateSkill` and update the scope accordingly.
    ///
    /// Call this after each tool dispatch round with the names and args
    /// of all tool calls from that round.
    pub fn update_from_tool_calls(
        &mut self,
        tool_calls: &[(String, serde_json::Value)],
        registry: &SkillRegistry,
    ) {
        for (name, args) in tool_calls {
            if name == "ActivateSkill" {
                if let Some(skill_name) = args.get("skill_name").and_then(|v| v.as_str()) {
                    if let Some(skill) = registry.get(skill_name) {
                        if skill.meta.allowed_tools.is_empty() {
                            self.allowed = None;
                        } else {
                            self.allowed = Some(skill.meta.allowed_tools.clone());
                        }
                    }
                    // If skill not found, don't change scope — the error
                    // message from activate_skill() is enough.
                }
            }
        }
    }

    /// Whether the scope is currently restricting tools.
    pub fn is_active(&self) -> bool {
        self.allowed.is_some()
    }

    /// Filter tool definitions to only include in-scope tools.
    ///
    /// Returns the full set (cloned) if no scope is active. Otherwise returns
    /// only tools whose names match `allowed_tools` or are meta-tools.
    pub fn filter_tool_defs(&self, tool_defs: &[ToolDefinition]) -> Vec<ToolDefinition> {
        match &self.allowed {
            None => tool_defs.to_vec(),
            Some(allowed) => tool_defs
                .iter()
                .filter(|td| allowed.contains(&td.name) || META_TOOLS.contains(&td.name.as_str()))
                .cloned()
                .collect(),
        }
    }

    /// Check if a tool call is allowed under the current scope.
    ///
    /// Returns `None` if allowed, or `Some(error_message)` if blocked.
    pub fn check_tool_call(&self, tool_name: &str) -> Option<String> {
        match &self.allowed {
            None => None,
            Some(allowed) => {
                if allowed.iter().any(|t| t == tool_name) || META_TOOLS.contains(&tool_name) {
                    None
                } else {
                    Some(format!(
                        "Tool '{tool_name}' is not allowed by the active skill scope. \
                         Allowed tools: {}. \
                         Use ActivateSkill to switch to a different skill.",
                        allowed.join(", ")
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{Skill, SkillMeta, SkillSource};

    fn make_registry() -> SkillRegistry {
        let mut registry = SkillRegistry::default();
        // Scoped skill
        registry.skills.insert(
            "scoped".to_string(),
            Skill {
                meta: SkillMeta {
                    name: "scoped".to_string(),
                    description: "Scoped".to_string(),
                    tags: vec![],
                    when_to_use: None,
                    allowed_tools: vec!["Read".to_string(), "Grep".to_string()],
                    user_invocable: true,
                    argument_hint: None,
                    source: SkillSource::BuiltIn,
                },
                content: "scoped content".to_string(),
            },
        );
        // Unscoped skill
        registry.add_builtin("unscoped", "Unscoped", None, "unscoped content");
        registry
    }

    fn tool_defs() -> Vec<ToolDefinition> {
        [
            "Read",
            "Grep",
            "Write",
            "Bash",
            "ActivateSkill",
            "ListSkills",
        ]
        .iter()
        .map(|n| ToolDefinition {
            name: n.to_string(),
            description: format!("{n} tool"),
            parameters: serde_json::json!({}),
        })
        .collect()
    }

    #[test]
    fn test_no_scope_passes_all_tools() {
        let scope = SkillToolScope::new();
        let defs = tool_defs();
        let filtered = scope.filter_tool_defs(&defs);
        assert_eq!(filtered.len(), defs.len());
    }

    #[test]
    fn test_activate_scoped_skill_filters_tools() {
        let registry = make_registry();
        let mut scope = SkillToolScope::new();
        let calls = vec![(
            "ActivateSkill".to_string(),
            serde_json::json!({"skill_name": "scoped"}),
        )];
        scope.update_from_tool_calls(&calls, &registry);

        assert!(scope.is_active());

        let defs = tool_defs();
        let filtered = scope.filter_tool_defs(&defs);
        let names: Vec<&str> = filtered.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Grep"));
        assert!(names.contains(&"ActivateSkill")); // meta-tool always present
        assert!(names.contains(&"ListSkills")); // meta-tool always present
        assert!(!names.contains(&"Write")); // not in allowed_tools
        assert!(!names.contains(&"Bash")); // not in allowed_tools
    }

    #[test]
    fn test_activate_unscoped_skill_clears_scope() {
        let registry = make_registry();
        let mut scope = SkillToolScope::new();

        // First activate scoped
        scope.update_from_tool_calls(
            &[(
                "ActivateSkill".to_string(),
                serde_json::json!({"skill_name": "scoped"}),
            )],
            &registry,
        );
        assert!(scope.is_active());

        // Then activate unscoped — scope should clear
        scope.update_from_tool_calls(
            &[(
                "ActivateSkill".to_string(),
                serde_json::json!({"skill_name": "unscoped"}),
            )],
            &registry,
        );
        assert!(!scope.is_active());
    }

    #[test]
    fn test_check_tool_call_allowed() {
        let registry = make_registry();
        let mut scope = SkillToolScope::new();
        scope.update_from_tool_calls(
            &[(
                "ActivateSkill".to_string(),
                serde_json::json!({"skill_name": "scoped"}),
            )],
            &registry,
        );

        assert!(scope.check_tool_call("Read").is_none());
        assert!(scope.check_tool_call("Grep").is_none());
        assert!(scope.check_tool_call("ActivateSkill").is_none()); // meta
        assert!(scope.check_tool_call("AskUser").is_none()); // meta

        let err = scope.check_tool_call("Write");
        assert!(err.is_some());
        assert!(err.unwrap().contains("not allowed"));
    }

    #[test]
    fn test_no_scope_allows_everything() {
        let scope = SkillToolScope::new();
        assert!(scope.check_tool_call("Write").is_none());
        assert!(scope.check_tool_call("Bash").is_none());
        assert!(scope.check_tool_call("anything").is_none());
    }

    #[test]
    fn test_unknown_skill_preserves_scope() {
        let registry = make_registry();
        let mut scope = SkillToolScope::new();

        // Activate scoped first
        scope.update_from_tool_calls(
            &[(
                "ActivateSkill".to_string(),
                serde_json::json!({"skill_name": "scoped"}),
            )],
            &registry,
        );
        assert!(scope.is_active());

        // Try activating non-existent skill — scope should not change
        scope.update_from_tool_calls(
            &[(
                "ActivateSkill".to_string(),
                serde_json::json!({"skill_name": "nope"}),
            )],
            &registry,
        );
        assert!(scope.is_active());
    }

    #[test]
    fn test_non_activate_calls_ignored() {
        let registry = make_registry();
        let mut scope = SkillToolScope::new();
        scope.update_from_tool_calls(
            &[(
                "Read".to_string(),
                serde_json::json!({"file_path": "foo.rs"}),
            )],
            &registry,
        );
        assert!(!scope.is_active());
    }
}
