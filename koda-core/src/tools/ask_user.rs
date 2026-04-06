//! AskUser tool — explicit clarification requests from the model.
//!
//! When the model needs information it cannot infer from context, it calls
//! this tool instead of guessing or stalling. The question appears in the TUI
//! menu area; the user types their answer and presses Enter.
//!
//! ## Parameters
//!
//! - **`question`** (required) — The question to ask the user
//!
//! ## When the model should use this
//!
//! - Ambiguous instructions ("fix the tests" — which tests?)
//! - Missing context (API keys, environment details)
//! - Design decisions that need human input
//!
//! ## When the model should NOT use this
//!
//! - Information available in the codebase (use Read/Grep instead)
//! - Binary yes/no confirmations (the approval flow handles those)
//!
//! Classified as `ReadOnly` — no side effects, no approval prompt.
//! Handled directly in `execute_tools_sequential` (needs `sink` + `cmd_rx`).

use crate::providers::ToolDefinition;
use serde_json::json;

/// Return tool definitions for the LLM.
pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "AskUser".to_string(),
        description: "Ask the user a specific question when you need clarification \
            before proceeding. Use this instead of guessing. \
            Keep the question short and direct. \
            Provide options when the answer is one of a known set of choices."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask the user"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional list of answer choices (omit for freeform)"
                }
            },
            "required": ["question"]
        }),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> ToolDefinition {
        definitions().remove(0)
    }

    #[test]
    fn test_definitions_returns_exactly_one_tool() {
        assert_eq!(definitions().len(), 1);
    }

    #[test]
    fn test_tool_name_is_ask_user() {
        assert_eq!(def().name, "AskUser");
    }

    #[test]
    fn test_description_is_non_empty() {
        assert!(!def().description.is_empty());
    }

    #[test]
    fn test_question_is_required() {
        let required = def().parameters["required"].clone();
        let required_fields: Vec<&str> = required
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required_fields.contains(&"question"));
    }

    #[test]
    fn test_options_is_not_required() {
        let required = def().parameters["required"].clone();
        let required_fields: Vec<&str> = required
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(!required_fields.contains(&"options"));
    }

    #[test]
    fn test_options_is_array_type() {
        let options_type = &def().parameters["properties"]["options"]["type"];
        assert_eq!(options_type.as_str().unwrap(), "array");
    }

    #[test]
    fn test_question_is_string_type() {
        let q_type = &def().parameters["properties"]["question"]["type"];
        assert_eq!(q_type.as_str().unwrap(), "string");
    }
}
