//! Microcompact — lightweight tool result aging without full compaction.
//!
//! Replaces old tool result content with a stub (`[Old tool result content cleared]`)
//! directly in the database. No LLM call, no API cost — just a SQL UPDATE.
//!
//! **Time-based trigger**: only fires when the gap since the last assistant
//! message exceeds a threshold (default 5 minutes). This prevents aggressive
//! clearing during active tool use and matches Claude Code's pattern where
//! microcompact only runs when the prompt cache has gone cold.
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
///
/// Claude Code uses 5 with a 60-minute gap threshold. We match their
/// keep-recent count.
const KEEP_RECENT: usize = 5;

/// Minimum idle gap (in seconds) since the last assistant message before
/// microcompact fires. During active tool use the model needs those results;
/// clearing them mid-turn is wasteful and confusing.
///
/// 5 minutes = user went for coffee, came back, sent a new message.
/// Claude Code uses 60 minutes (tied to Anthropic's prompt cache TTL).
/// We use a shorter gap because koda has no server-side cache to protect.
const GAP_THRESHOLD_SECS: i64 = 300;

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
/// Only fires when the gap since the last assistant message exceeds
/// `GAP_THRESHOLD_SECS`. Returns `None` if the trigger doesn't fire
/// or nothing was cleared.
pub async fn microcompact_session(
    db: &Database,
    session_id: &str,
) -> Result<Option<MicrocompactResult>> {
    // Check the time-based trigger first — skip the heavy scan if idle gap
    // hasn't been reached.
    let gap = db.seconds_since_last_assistant(session_id).await?;
    match gap {
        None => return Ok(None), // No assistant messages yet.
        Some(s) if s < GAP_THRESHOLD_SECS => return Ok(None),
        _ => {} // Gap exceeded — proceed.
    }

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
            && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
        {
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
            full_content: None,
            tool_calls: tool_calls.map(String::from),
            tool_call_id: tool_call_id.map(String::from),
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
            thinking_content: None,
            created_at: None,
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

    /// Integration test: verifies microcompact clears old results in a real SQLite DB,
    /// but only when the time-based trigger fires (last assistant msg is old enough).
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
            let mid = db
                .insert_message(&session, &Role::Assistant, None, Some(&tc_json), None, None)
                .await
                .unwrap();
            // Mark complete — these represent finished turns that load_context must see.
            db.mark_message_complete(mid).await.unwrap();
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

        // Should NOT trigger — last assistant message is fresh (just inserted).
        let result = microcompact_session(&db, &session).await.unwrap();
        assert!(result.is_none(), "should not trigger for fresh messages");

        // Backdate the last assistant message so the time-based trigger fires.
        sqlx::query(
            "UPDATE messages SET created_at = datetime('now', '-10 minutes') \
             WHERE session_id = ? AND role = 'assistant' \
             AND id = (SELECT MAX(id) FROM messages WHERE session_id = ? AND role = 'assistant')",
        )
        .bind(&session)
        .bind(&session)
        .execute(db.pool())
        .await
        .unwrap();

        // NOW it should trigger.
        let result = microcompact_session(&db, &session).await.unwrap();
        assert!(result.is_some(), "should trigger after gap threshold");
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

    // ── estimate_tokens ───────────────────────────────────────────────

    #[test]
    fn test_estimate_tokens_proportional_to_chars() {
        let short = estimate_tokens("hello");
        let long = estimate_tokens(&"x".repeat(400));
        assert!(long > short, "more chars should estimate more tokens");
    }

    #[test]
    fn test_estimate_tokens_empty_string() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_below_min_threshold() {
        // Content shorter than MIN_TOKENS_TO_CLEAR chars should estimate < threshold.
        let tiny = "hi";
        let tokens = estimate_tokens(tiny);
        assert!(
            tokens < MIN_TOKENS_TO_CLEAR,
            "tiny content ({tokens} tokens) should be below MIN_TOKENS_TO_CLEAR ({MIN_TOKENS_TO_CLEAR})"
        );
    }

    // ── is_compactable ──────────────────────────────────────────────

    #[test]
    fn test_is_not_compactable_write() {
        assert!(!is_compactable("Write"));
        assert!(!is_compactable("write"));
    }

    #[test]
    fn test_is_not_compactable_edit() {
        assert!(!is_compactable("Edit"));
        assert!(!is_compactable("edit"));
    }

    #[test]
    fn test_is_not_compactable_unknown_tool() {
        assert!(!is_compactable("FancyCustomTool"));
        assert!(!is_compactable(""));
    }

    // ── diagnosis ───────────────────────────────────────────────────

    #[test]
    fn test_diagnosis_returns_none_below_token_threshold() {
        // A small tool result (<500 tokens) should not trigger diagnosis.
        let tc = r#"[{"id":"tc_1","function_name":"Bash","arguments":"{}"}]"#;
        let messages = vec![
            msg(1, Role::Assistant, None, Some(tc), None),
            msg(2, Role::Tool, Some("tiny result"), None, Some("tc_1")),
        ];
        assert!(diagnosis(&messages).is_none());
    }

    #[test]
    fn test_diagnosis_includes_compactable_tools_only() {
        // Write tool results should NOT appear in diagnosis even if large.
        let tc_write = r#"[{"id":"tc_w","function_name":"Write","arguments":"{}"}]"#;
        let tc_read = r#"[{"id":"tc_r","function_name":"Read","arguments":"{}"}]"#;
        let big = "X".repeat(3000);
        let messages = vec![
            msg(1, Role::Assistant, None, Some(tc_write), None),
            msg(2, Role::Tool, Some(&big), None, Some("tc_w")),
            msg(3, Role::Assistant, None, Some(tc_read), None),
            msg(4, Role::Tool, Some(&big), None, Some("tc_r")),
        ];
        let d = diagnosis(&messages);
        assert!(d.is_some());
        let text = d.unwrap();
        assert!(
            !text.contains("Write"),
            "Write should not appear in diagnosis"
        );
        assert!(text.contains("Read"), "Read should appear in diagnosis");
    }

    #[test]
    fn test_diagnosis_returns_none_when_all_tools_non_compactable() {
        // Only Write results — nothing compactable to diagnose.
        let tc = r#"[{"id":"tc_w","function_name":"Write","arguments":"{}"}]"#;
        let big = "W".repeat(3000);
        let messages = vec![
            msg(1, Role::Assistant, None, Some(tc), None),
            msg(2, Role::Tool, Some(&big), None, Some("tc_w")),
        ];
        assert!(diagnosis(&messages).is_none());
    }

    // ── build_tool_id_map edge cases ──────────────────────────────────

    #[test]
    fn test_build_tool_id_map_accepts_name_key_variant() {
        // Some providers emit "name" instead of "function_name".
        let tc = r#"[{"id":"tc_x","name":"Grep","arguments":"{}"}]"#;
        let messages = vec![msg(1, Role::Assistant, None, Some(tc), None)];
        let map = build_tool_id_map(&messages);
        assert_eq!(map.get("tc_x").map(|s| s.as_str()), Some("Grep"));
    }

    #[test]
    fn test_build_tool_id_map_ignores_non_assistant_messages() {
        let tc = r#"[{"id":"tc_y","function_name":"Bash","arguments":"{}"}]"#;
        // Tool role message with tool_calls JSON — should be ignored.
        let messages = vec![msg(1, Role::Tool, None, Some(tc), None)];
        let map = build_tool_id_map(&messages);
        assert!(map.is_empty());
    }
}
