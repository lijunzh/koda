//! Microcompact — lightweight tool result aging without full compaction.
//!
//! Replaces old tool result content with a stub (`[Old tool result content cleared]`)
//! directly in the database. No LLM call, no API cost — just a SQL UPDATE.
//!
//! This keeps context lean between full compactions. A session with 20 file reads
//! only keeps the N most recent results; older ones become stubs.
//!
//! Inspired by Claude Code's `microCompact.ts`.

use crate::context_analysis;
use crate::db::Database;
use crate::inference_helpers::CHARS_PER_TOKEN;
use crate::persistence::{Message, Persistence, Role};
use anyhow::Result;
use std::collections::HashMap;

/// Stub text that replaces cleared tool results.
pub const CLEARED_MESSAGE: &str = "[Old tool result content cleared]";

/// Tools whose results can be safely cleared (output is re-obtainable).
const COMPACTABLE_TOOLS: &[&str] = &[
    "Read",
    "read",
    "Bash",
    "bash",
    "Grep",
    "grep",
    "Glob",
    "glob",
    "ListFiles",
    "list_files",
    "WebSearch",
    "web_search",
    "WebFetch",
    "web_fetch",
];

/// Number of most-recent compactable tool results to keep intact.
const KEEP_RECENT: usize = 6;

/// Minimum token size for a tool result to be worth clearing.
/// Don't bother clearing tiny results — the overhead of the stub is comparable.
const MIN_TOKENS_TO_CLEAR: usize = 50;

/// Result of a microcompact pass.
#[derive(Debug, Clone)]
pub struct MicrocompactResult {
    /// Number of tool results cleared.
    pub cleared: usize,
    /// Estimated tokens saved.
    pub tokens_saved: usize,
}

/// Run microcompact on a session — clear old compactable tool results.
///
/// This is cheap (no LLM call) and idempotent. Safe to run every turn.
///
/// Returns `None` if nothing was cleared (already clean or too few results).
pub async fn microcompact_session(
    db: &Database,
    session_id: &str,
) -> Result<Option<MicrocompactResult>> {
    let history = db.load_context(session_id).await?;
    if history.len() < KEEP_RECENT + 2 {
        return Ok(None);
    }

    // Build tool_call_id → tool_name map from assistant messages.
    let id_to_tool = build_tool_id_map(&history);

    // Collect compactable tool result message IDs in chronological order.
    let compactable: Vec<CompactableResult> = history
        .iter()
        .filter_map(|msg| {
            if msg.role != Role::Tool {
                return None;
            }
            let tool_call_id = msg.tool_call_id.as_deref()?;
            let tool_name = id_to_tool.get(tool_call_id)?;
            if !is_compactable(tool_name) {
                return None;
            }
            // Skip already-cleared results.
            let content = msg.content.as_deref().unwrap_or("");
            if content == CLEARED_MESSAGE {
                return None;
            }
            let tokens = estimate_tokens(content);
            if tokens < MIN_TOKENS_TO_CLEAR {
                return None;
            }
            Some(CompactableResult {
                message_id: msg.id,
                tokens,
            })
        })
        .collect();

    if compactable.len() <= KEEP_RECENT {
        return Ok(None);
    }

    // Keep the last KEEP_RECENT, clear the rest.
    let to_clear = &compactable[..compactable.len() - KEEP_RECENT];

    let mut tokens_saved = 0usize;
    let mut cleared = 0usize;

    for batch in to_clear.chunks(100) {
        let ids: Vec<i64> = batch.iter().map(|c| c.message_id).collect();
        db.clear_message_content(&ids, CLEARED_MESSAGE).await?;
        tokens_saved += batch.iter().map(|c| c.tokens).sum::<usize>();
        cleared += batch.len();
    }

    if cleared == 0 {
        return Ok(None);
    }

    tracing::info!("Microcompact: cleared {cleared} tool results, saved ~{tokens_saved} tokens");

    Ok(Some(MicrocompactResult {
        cleared,
        tokens_saved,
    }))
}

/// Identifies the best candidates for microcompact using context analysis.
///
/// Returns a human-readable hint for diagnostics (e.g., "Bash: ~8000 tok, Read: ~3000 tok").
pub fn diagnosis(messages: &[Message]) -> Option<String> {
    let analysis = context_analysis::analyze_context(messages);
    let top = analysis.top_tool_results(3);
    if top.is_empty() || analysis.total_tool_result_tokens() < 500 {
        return None;
    }

    let parts: Vec<String> = top
        .iter()
        .filter(|(name, _)| is_compactable(name))
        .map(|(name, tokens)| format!("{name}: ~{tokens} tok"))
        .collect();

    if parts.is_empty() {
        return None;
    }

    Some(parts.join(", "))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct CompactableResult {
    message_id: i64,
    tokens: usize,
}

fn is_compactable(tool_name: &str) -> bool {
    COMPACTABLE_TOOLS.contains(&tool_name)
}

fn estimate_tokens(content: &str) -> usize {
    (content.len() as f64 / CHARS_PER_TOKEN) as usize
}

/// Build a map from tool_call_id → tool_name by scanning assistant messages.
fn build_tool_id_map(messages: &[Message]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for msg in messages {
        if msg.role == Role::Assistant
            && let Some(ref tc_json) = msg.tool_calls
                && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json) {
                    for call in &calls {
                        let id = call.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                        let name = call
                            .get("function_name")
                            .or_else(|| call.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        if !id.is_empty() {
                            map.insert(id.to_string(), name.to_string());
                        }
                    }
                }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Message, Role};

    fn msg(
        id: i64,
        role: Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
    ) -> Message {
        Message {
            id,
            session_id: String::new(),
            role,
            content: content.map(String::from),
            tool_calls: tool_calls.map(String::from),
            tool_call_id: tool_call_id.map(String::from),
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
        }
    }

    #[test]
    fn test_is_compactable() {
        assert!(is_compactable("Read"));
        assert!(is_compactable("Bash"));
        assert!(is_compactable("Grep"));
        assert!(is_compactable("Glob"));
        assert!(is_compactable("WebSearch"));
        assert!(is_compactable("WebFetch"));
        assert!(!is_compactable("InvokeAgent"));
        assert!(!is_compactable("TodoWrite"));
        assert!(!is_compactable("AskUser"));
    }

    #[test]
    fn test_build_tool_id_map() {
        let tc = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"},{"id":"tc_2","function_name":"Bash","arguments":"{}"}]"#;
        let messages = vec![msg(1, Role::Assistant, None, Some(tc), None)];
        let map = build_tool_id_map(&messages);
        assert_eq!(map.get("tc_1").unwrap(), "Read");
        assert_eq!(map.get("tc_2").unwrap(), "Bash");
    }

    #[test]
    fn test_already_cleared_skipped() {
        let tc = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
        let messages = vec![
            msg(1, Role::Assistant, None, Some(tc), None),
            msg(2, Role::Tool, Some(CLEARED_MESSAGE), None, Some("tc_1")),
        ];
        let _id_map = build_tool_id_map(&messages);
        let compactable: Vec<_> = messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter(|m| {
                let content = m.content.as_deref().unwrap_or("");
                content != CLEARED_MESSAGE
            })
            .collect();
        assert!(compactable.is_empty());
    }

    #[test]
    fn test_diagnosis_with_results() {
        let tc1 = r#"[{"id":"tc_1","function_name":"Read","arguments":"{}"}]"#;
        let tc2 = r#"[{"id":"tc_2","function_name":"Bash","arguments":"{}"}]"#;
        let long = "x".repeat(2000);
        let messages = vec![
            msg(1, Role::User, Some("hi"), None, None),
            msg(2, Role::Assistant, None, Some(tc1), None),
            msg(3, Role::Tool, Some(&long), None, Some("tc_1")),
            msg(4, Role::Assistant, None, Some(tc2), None),
            msg(5, Role::Tool, Some(&long), None, Some("tc_2")),
        ];
        let diag = diagnosis(&messages);
        assert!(diag.is_some());
        let text = diag.unwrap();
        assert!(text.contains("Read") || text.contains("Bash"));
    }

    #[test]
    fn test_diagnosis_empty() {
        let messages = vec![
            msg(1, Role::User, Some("hi"), None, None),
            msg(2, Role::Assistant, Some("hello"), None, None),
        ];
        assert!(diagnosis(&messages).is_none());
    }

    /// Integration test: verifies microcompact clears old results in a real SQLite DB.
    #[tokio::test]
    async fn test_microcompact_session_integration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = crate::db::Database::open(&db_path).await.unwrap();
        let session = db.create_session("default", tmp.path()).await.unwrap();

        let long_content = "x".repeat(500);

        // Insert KEEP_RECENT + 3 compactable tool calls (Read).
        for i in 0..(KEEP_RECENT + 3) {
            let tc_id = format!("tc_{i}");
            let tc_json =
                format!(r#"[{{"id":"{tc_id}","function_name":"Read","arguments":"{{}}"}}]"#);
            // Assistant with tool_calls
            db.insert_message(&session, &Role::Assistant, None, Some(&tc_json), None, None)
                .await
                .unwrap();
            // Tool result
            db.insert_message(
                &session,
                &Role::Tool,
                Some(&long_content),
                None,
                Some(&tc_id),
                None,
            )
            .await
            .unwrap();
        }

        // Run microcompact
        let result = microcompact_session(&db, &session).await.unwrap();
        assert!(result.is_some());
        let mc = result.unwrap();
        assert_eq!(mc.cleared, 3); // 3 oldest should be cleared
        assert!(mc.tokens_saved > 0);

        // Verify: load context and check that old results are stubs.
        let history = db.load_context(&session).await.unwrap();
        let tool_msgs: Vec<_> = history.iter().filter(|m| m.role == Role::Tool).collect();

        // First 3 should be cleared
        for m in &tool_msgs[..3] {
            assert_eq!(m.content.as_deref().unwrap(), CLEARED_MESSAGE);
        }
        // Last KEEP_RECENT should be intact
        for m in &tool_msgs[3..] {
            assert_eq!(m.content.as_deref().unwrap(), long_content);
        }

        // Run again — should be idempotent (nothing more to clear)
        let result2 = microcompact_session(&db, &session).await.unwrap();
        assert!(result2.is_none());
    }
}
