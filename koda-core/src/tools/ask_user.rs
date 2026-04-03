//! AskUser tool: explicit clarification requests from the model.
//!
//! When the model needs information it cannot infer from context, it calls
//! this tool instead of guessing or stalling. The question appears in the TUI
//! menu area; the user types their answer and presses Enter.
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
