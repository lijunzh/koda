//! RecallContext — on-demand conversation history retrieval.
//!
//! Strong-tier only. Allows the model to page in older conversation
//! context that was dropped from the sliding window.

use crate::db::Database;
use crate::persistence::Persistence;
use crate::providers::ToolDefinition;
use serde_json::json;

/// RecallContext tool definition.
pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "RecallContext".to_string(),
        description: "Recall earlier conversation context that may have scrolled \
            out of your current window. Use when you need to remember what was \
            discussed or decided earlier in the session."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search term to find in conversation history"
                },
                "turn": {
                    "type": "integer",
                    "description": "Specific turn number to recall (1-based)"
                }
            }
        }),
    }
}

/// Execute RecallContext: search or fetch specific turns from history.
pub async fn recall_context(db: &Database, session_id: &str, args: &serde_json::Value) -> String {
    let query = args["query"].as_str();
    let turn = args["turn"].as_u64();

    if query.is_none() && turn.is_none() {
        return "Provide either 'query' (search term) or 'turn' (number) to recall context."
            .to_string();
    }

    // Load full history (no token limit)
    let history = match db.load_all_messages(session_id).await {
        Ok(msgs) => msgs,
        Err(e) => return format!("Failed to load history: {e}"),
    };

    if history.is_empty() {
        return "No conversation history found.".to_string();
    }

    // Fetch by turn number
    if let Some(turn_num) = turn {
        let idx = turn_num.saturating_sub(1) as usize;
        if idx >= history.len() {
            return format!(
                "Turn {} does not exist. Session has {} messages.",
                turn_num,
                history.len()
            );
        }
        let msg = &history[idx];
        let content = msg.content.as_deref().unwrap_or("(no content)");
        // Truncate very long messages
        let display = if content.len() > 2000 {
            format!(
                "{}... [truncated, {} chars total]",
                &content[..2000],
                content.len()
            )
        } else {
            content.to_string()
        };
        return format!("## Turn {} ({})\n\n{}", turn_num, msg.role, display);
    }

    // Search by query
    if let Some(q) = query {
        let q_lower = q.to_lowercase();
        let mut matches = Vec::new();
        for (i, msg) in history.iter().enumerate() {
            if let Some(ref content) = msg.content
                && content.to_lowercase().contains(&q_lower)
            {
                let snippet = extract_snippet(content, &q_lower, 200);
                matches.push(format!("**Turn {} ({}):** {}\n", i + 1, msg.role, snippet));
            }
        }

        if matches.is_empty() {
            return format!("No matches for '{q}' in conversation history.");
        }

        // Cap at 10 matches
        let total = matches.len();
        let shown: Vec<_> = matches.into_iter().take(10).collect();
        let mut result = format!("## Found {total} matches for '{q}'\n\n");
        result.push_str(&shown.join("\n"));
        if total > 10 {
            result.push_str(&format!("\n... and {} more matches\n", total - 10));
        }
        return result;
    }

    "Provide 'query' or 'turn' parameter.".to_string()
}

/// Extract a snippet around the first match, with context.
fn extract_snippet(text: &str, query: &str, max_len: usize) -> String {
    let lower = text.to_lowercase();
    let pos = match lower.find(query) {
        Some(p) => p,
        None => return text.chars().take(max_len).collect(),
    };

    let start = pos.saturating_sub(50);
    let end = (pos + query.len() + 150).min(text.len());
    let snippet = &text[start..end];

    if start > 0 || end < text.len() {
        format!("...{snippet}...")
    } else {
        snippet.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_definition() {
        let def = definition();
        assert_eq!(def.name, "RecallContext");
    }

    #[test]
    fn test_extract_snippet_found() {
        let text = "The quick brown fox jumps over the lazy dog";
        let snippet = extract_snippet(text, "fox", 100);
        assert!(snippet.contains("fox"));
    }

    #[test]
    fn test_extract_snippet_not_found() {
        let text = "hello world";
        let snippet = extract_snippet(text, "xyz", 100);
        assert_eq!(snippet, "hello world");
    }
}
