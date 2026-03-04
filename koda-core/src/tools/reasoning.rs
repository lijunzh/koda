//! ShareReasoning tool — lets any model externalize chain-of-thought.
//!
//! Works as a universal reasoning fallback for models without native
//! thinking support (Gemini, Groq, local models, etc.). When native
//! thinking is active, this tool is automatically excluded.

use crate::providers::ToolDefinition;
use anyhow::Result;
use serde_json::Value;

pub fn definitions() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "ShareReasoning".to_string(),
        description: "Share your step-by-step reasoning about the current task. \
            Use this to think through complex problems before acting. \
            Call this tool when you need to plan, analyze trade-offs, \
            or work through a multi-step problem."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Brief title for this reasoning block"
                },
                "reasoning": {
                    "type": "string",
                    "description": "Your detailed step-by-step reasoning"
                }
            },
            "required": ["reasoning"]
        }),
    }]
}

pub async fn share_reasoning(_args: &Value) -> Result<String> {
    Ok("Reasoning noted.".to_string())
}
