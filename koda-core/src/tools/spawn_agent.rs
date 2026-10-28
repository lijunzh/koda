//! `SpawnAgent` — codex v2 peer-style spawn tool (#1325 Phase 5a).
//!
//! Mirrors codex's `spawn_agent` shape: `task_name` + `message` (no
//! `background` flag, no `fork_turns`) so prompts written against
//! codex v2 are structurally compatible with koda.
//!
//! ## Relationship to `InvokeAgent`
//!
//! Both tools dispatch to the same `execute_sub_agent` machinery in
//! `sub_agent_dispatch.rs`. The difference is surface shape and naming:
//!
//! | | `InvokeAgent` | `SpawnAgent` |
//! |---|---|---|
//! | `agent_name` field | explicit | `agent` (built-in only for now) |
//! | task description | `prompt` | `message` |
//! | style | koda-native | codex v2 / Claude Code compatible |
//!
//! `SpawnAgent` is intercepted by `tool_dispatch.rs` exactly like
//! `InvokeAgent`. The actual dispatch path is reused; this file just
//! provides the schema and the catalog stub so the LLM can call it.
//!
//! ## Phase 5a scope
//!
//! Phase 5a adds the tool shape + dispatch wiring. Phase 5b will retire
//! `WaitTask`/`ListBackgroundTasks`/`CancelTask` once all callers migrate.

use crate::providers::ToolDefinition;
use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "SpawnAgent".to_string(),
        description: "Delegate a task to a named sub-agent and get its final answer back as \
            the tool result.\n\
            \n\
            This is an alias for `InvokeAgent` with the codex v2 / Claude Code `spawn_agent` \
            argument shape: same dispatch path, same execution model. The call BLOCKS until \
            the sub-agent finishes, and the result IS the sub-agent's final answer. \
            Nothing runs in the background and there is no task id to wait on.\n\
            \n\
            `task_name` identifies the agent configuration to load (same as \
            `InvokeAgent`'s `agent_name`). Use `ListAgents` to see available agents.\n\
            \n\
            `message` is the task prompt handed to the sub-agent."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Agent configuration name (e.g. `explore`, `task`, `fork`). \
                        Use `ListAgents` to see what is available."
                },
                "message": {
                    "type": "string",
                    "description": "The task prompt to hand to the sub-agent."
                }
            },
            "required": ["task_name", "message"]
        }),
    }]
}

/// `SpawnAgent` — intercepted by `tool_dispatch.rs`.
///
/// The trait impl exists only to register the tool in the catalog and
/// keep the dispatch table complete. Actual execution is handled by
/// `tool_dispatch.rs` (which re-maps `task_name`/`message` to the
/// `agent_name`/`prompt` fields that `execute_sub_agent` expects).
pub struct SpawnAgentTool;

#[async_trait]
impl Tool for SpawnAgentTool {
    fn name(&self) -> &'static str {
        "SpawnAgent"
    }

    fn definition(&self) -> ToolDefinition {
        definitions()
            .into_iter()
            .next()
            .expect("spawn_agent::definitions() must be non-empty")
    }

    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        // Sub-agents inherit the parent's approval mode; classification
        // here is a placeholder — dispatch never reaches this impl.
        ToolEffect::ReadOnly
    }

    async fn execute(&self, _ctx: &ToolExecCtx<'_>, _args: &serde_json::Value) -> ToolResult {
        ToolResult {
            output: "SpawnAgent is handled by the inference loop.".to_string(),
            success: false,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_matches_definition() {
        let t = SpawnAgentTool;
        assert_eq!(t.name(), "SpawnAgent");
        assert_eq!(t.definition().name, "SpawnAgent");
    }

    #[test]
    fn classify_is_read_only() {
        let t = SpawnAgentTool;
        assert_eq!(t.classify(&serde_json::Value::Null), ToolEffect::ReadOnly);
    }

    #[test]
    fn definition_requires_task_name_and_message() {
        let def = SpawnAgentTool.definition();
        let required = def.parameters["required"]
            .as_array()
            .expect("required array");
        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            required_names.contains(&"task_name"),
            "task_name must be required"
        );
        assert!(
            required_names.contains(&"message"),
            "message must be required"
        );
    }

    #[test]
    fn definition_matches_the_synchronous_dispatch() {
        // #1366: `SpawnAgent` is dispatched exactly like `InvokeAgent`
        // (it blocks and returns the final answer). The description once
        // promised an immediate task id and mailbox delivery, which sent
        // the model to `WaitForMail` after every call.
        let desc = &definitions()[0].description;
        assert!(
            desc.contains("BLOCKS") && desc.contains("final answer"),
            "got:\n{desc}"
        );
        assert!(desc.contains("alias for `InvokeAgent`"), "got:\n{desc}");
        for ghost in [
            "Returns IMMEDIATELY",
            "work on a task in the background",
            "runs concurrently",
            "delivered to your mailbox",
        ] {
            assert!(
                !desc.contains(ghost),
                "stale async wording {ghost:?}; got:\n{desc}"
            );
        }
    }

    #[test]
    fn definitions_non_empty() {
        assert!(!definitions().is_empty());
        assert_eq!(definitions()[0].name, "SpawnAgent");
    }
}
