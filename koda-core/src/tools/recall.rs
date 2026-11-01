//! RecallContext — on-demand conversation history retrieval.
//!
//! Allows the model to page in older conversation context that was dropped
//! from the sliding window after compaction or microcompact.
//!
//! ## When it's used
//!
//! After compaction summarizes old messages, the model may need specific
//! details (e.g., the exact error message from an earlier test run). Rather
//! than re-running the command, it can recall the original tool result.
//!
//! ## Availability
//!
//! Strong-tier models only — cheaper models don't benefit enough from
//! the extra context to justify the cost.

use crate::db::Database;
use crate::persistence::Persistence;
use crate::providers::ToolDefinition;
use serde_json::json;
use std::ops::Range;

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
        // Prefer full_content (untruncated Bash output) over content (summary).
        let content = msg
            .full_content
            .as_deref()
            .or(msg.content.as_deref())
            .unwrap_or("(no content)");
        // Truncate very long messages
        let display = if content.len() > 2000 {
            format!(
                "{}... [truncated, {} chars total]",
                &content[..content.floor_char_boundary(2000)],
                content.len()
            )
        } else {
            content.to_string()
        };
        return format!("## Turn {} ({})\n\n{}", turn_num, msg.role, display);
    }

    // Search by query — searches both content and full_content.
    if let Some(q) = query {
        let q_lower = q.to_lowercase();
        let mut matches = Vec::new();
        for (i, msg) in history.iter().enumerate() {
            // Search full_content first (has untruncated Bash output),
            // fall back to content (summary / normal tool output).
            let searchable = msg.full_content.as_deref().or(msg.content.as_deref());
            if let Some(text) = searchable
                && text.to_lowercase().contains(&q_lower)
            {
                let snippet = extract_snippet(text, &q_lower, 200);
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

/// Map a byte range in `text.to_lowercase()` back to the characters of `text`
/// it came from. Lowercasing can change a string's byte length, so an offset
/// into the lowercased copy doesn't fit the original.
///
/// Counting character by character gives the same lengths as `to_lowercase`:
/// its one special case, final sigma, swaps `σ` for `ς`, both two bytes.
fn original_range(text: &str, lower: Range<usize>) -> Range<usize> {
    let mut lower_pos = 0;
    let mut start = None;
    for (i, c) in text.char_indices() {
        let lower_next = lower_pos + c.to_lowercase().map(char::len_utf8).sum::<usize>();
        if start.is_none() && lower_next > lower.start {
            start = Some(i);
        }
        if lower_next >= lower.end {
            return start.unwrap_or(i)..i + c.len_utf8();
        }
        lower_pos = lower_next;
    }
    start.unwrap_or(text.len())..text.len()
}

/// Extract a snippet around the first match, with context.
///
/// `query` is already lowercased. This searches the same lowercased text as
/// the filter in `recall_context`, so a message that passes it shows its match.
fn extract_snippet(text: &str, query: &str, max_len: usize) -> String {
    let lower = text.to_lowercase();
    let pos = match lower.find(query) {
        Some(p) => p,
        None => return text.chars().take(max_len).collect(),
    };

    let found = original_range(text, pos..pos + query.len());
    let start = text.floor_char_boundary(found.start.saturating_sub(50));
    let end = text.floor_char_boundary(found.end + 150);
    let snippet = &text[start..end];

    if start > 0 || end < text.len() {
        format!("...{snippet}...")
    } else {
        snippet.to_string()
    }
}

// =============================================================
// Tool trait implementation (#1265 item 5, PR-7/N).
//
// `RecallContext` is read-only — it queries the session DB for
// previously-seen full-output content.
// =============================================================

use crate::tools::{Tool, ToolEffect, ToolExecCtx, ToolResult};
use async_trait::async_trait;

/// `RecallContext` — fetch full-output snippets from session history.
pub struct RecallContextTool;

#[async_trait]
impl Tool for RecallContextTool {
    fn name(&self) -> &'static str {
        "RecallContext"
    }
    fn definition(&self) -> ToolDefinition {
        definition()
    }
    fn classify(&self, _args: &serde_json::Value) -> ToolEffect {
        ToolEffect::ReadOnly
    }
    async fn execute(&self, ctx: &ToolExecCtx<'_>, args: &serde_json::Value) -> ToolResult {
        let Some((db, sid)) = ctx.session else {
            return ToolResult {
                output: "RecallContext requires an active session.".to_string(),
                success: true,
                full_output: None,
            };
        };
        // recall_context returns a `String` (not Result) — always success.
        ToolResult {
            output: recall_context(db, sid, args).await,
            success: true,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{Persistence, Role};
    use serde_json::json;

    // ── Trait invariants (#1265 PR-7) ─────────────────────────

    /// Pre-#1265 the no-session path returned a successful tool
    /// result with a self-explanatory message (`Ok("...")`). The
    /// trait must preserve that exactly — same message, same
    /// success flag.
    #[tokio::test]
    async fn recall_no_session_returns_graceful_message() {
        let t = RecallContextTool;
        let tmp = tempfile::tempdir().unwrap();
        let cache = crate::tools::FileReadCache::default();
        let fs = koda_sandbox::fs::LocalFileSystem;
        let caps = crate::output_caps::OutputCaps::for_context(100_000);
        let bg = crate::tools::bg_process::BgRegistry::new();
        let trust = crate::trust::TrustMode::Safe;
        let policy = koda_sandbox::SandboxPolicy::default();
        let skills = crate::skills::SkillRegistry::default();
        let ctx = crate::tools::ToolExecCtx::for_test(
            tmp.path(),
            &cache,
            &fs,
            &caps,
            &bg,
            &trust,
            &policy,
            &skills,
        );
        let r = t.execute(&ctx, &json!({})).await;
        assert!(r.success);
        assert_eq!(r.output, "RecallContext requires an active session.");
    }

    #[test]
    fn recall_tool_metadata() {
        let t = RecallContextTool;
        assert_eq!(t.name(), "RecallContext");
        assert_eq!(t.definition().name, "RecallContext");
        assert_eq!(
            t.classify(&serde_json::json!({})),
            crate::tools::ToolEffect::ReadOnly,
        );
        assert!(t.extract_undo_path(&serde_json::json!({})).is_none());
    }

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

    #[test]
    fn test_extract_snippet_at_start_no_leading_ellipsis() {
        // Match at position 0 — no leading context, no "..."
        let text = "match at the start and some more text here";
        let snippet = extract_snippet(text, "match", 100);
        assert!(
            !snippet.starts_with("..."),
            "no leading ellipsis when at start"
        );
        assert!(snippet.contains("match"));
    }

    #[test]
    fn test_extract_snippet_mid_text_has_ellipsis() {
        // 100 chars of padding before 'needle' forces start > 0
        let text = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let snippet = extract_snippet(&text, "needle", 200);
        assert!(
            snippet.starts_with("..."),
            "should have leading ellipsis: {snippet}"
        );
        assert!(
            snippet.ends_with("..."),
            "should have trailing ellipsis: {snippet}"
        );
    }

    #[test]
    fn test_extract_snippet_not_found_truncated_at_max_len() {
        // When no match, extract_snippet returns up to max_len chars.
        let text = "a".repeat(500);
        let snippet = extract_snippet(&text, "nothere", 50);
        assert_eq!(snippet.chars().count(), 50);
    }

    #[test]
    fn test_extract_snippet_empty_text() {
        let snippet = extract_snippet("", "query", 100);
        assert_eq!(snippet, "");
    }

    #[test]
    fn test_extract_snippet_query_is_case_lowered() {
        // extract_snippet expects the query to already be lowercased
        // (the caller lower-cases both text and query).
        let text = "Error: file not found at line 42";
        let lower_q = "error";
        let snippet = extract_snippet(text, lower_q, 200);
        assert!(snippet.contains("Error"));
    }

    // ── Unicode ───────────────────────────────────────────────

    #[test]
    fn extract_snippet_cut_inside_a_character_does_not_panic() {
        let text = "é".repeat(30) + "xneedle" + &"界".repeat(100);
        let snippet = extract_snippet(&text, "needle", 200);
        assert!(snippet.contains("needle"), "{snippet}");
    }

    /// `İ` lowercases to more bytes, so a lowercased offset overshoots.
    #[test]
    fn extract_snippet_handles_case_mapping_that_changes_length() {
        let text = "İ".repeat(40) + "Needle" + &"a".repeat(20);
        let snippet = extract_snippet(&text, "needle", 200);
        assert!(snippet.contains("Needle"), "{snippet}");
    }

    #[test]
    fn extract_snippet_keeps_a_match_that_case_folding_misses() {
        let text = "a".repeat(250) + "İSTANBUL" + &"x".repeat(20);
        let query = "İstanbul".to_lowercase();
        assert!(text.to_lowercase().contains(&query));
        let snippet = extract_snippet(&text, &query, 200);
        assert!(snippet.contains("İSTANBUL"), "{snippet}");
    }

    /// The Kelvin sign lowercases to fewer bytes, `İ` to more, and final sigma
    /// is `to_lowercase`'s one context-dependent case.
    #[test]
    fn original_range_covers_the_matched_characters() {
        for (text, query, matched) in [
            ("\u{212A}\u{212A}Needle", "needle", "Needle"),
            ("İİNeedle", "needle", "Needle"),
            ("aİSTANBULb", "i\u{307}stanbul", "İSTANBUL"),
            ("x ΟΔΟΣ y", "οδος", "ΟΔΟΣ"),
            ("ends here", "here", "here"),
        ] {
            let lower = text.to_lowercase();
            let pos = lower.find(query).unwrap();
            let range = original_range(text, pos..pos + query.len());
            assert_eq!(&text[range], matched, "{text}");
        }
    }

    /// What `original_range` relies on, checked on this toolchain's Unicode
    /// tables: both ways of lowercasing give the same length, for every
    /// character, at the end of a word and inside one.
    #[test]
    fn char_by_char_lowercasing_matches_to_lowercase_in_length() {
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            for text in [format!("x{c}"), format!("x{c}x")] {
                let by_char: usize = text
                    .chars()
                    .map(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>())
                    .sum();
                assert_eq!(by_char, text.to_lowercase().len(), "U+{:04X}", c as u32);
            }
        }
    }

    /// A query can end inside what one character lowercases to. The range
    /// still has to cover that whole character.
    #[test]
    fn original_range_rounds_out_to_whole_characters() {
        let text = "aİb";
        let lower = text.to_lowercase();
        let pos = lower.find('i').unwrap();
        assert_eq!(&text[original_range(text, pos..pos + 1)], "İ");
    }

    // ── recall_context integration tests (requires DB) ─────────────────────

    async fn test_db() -> (Database, tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("recall_test.db"))
            .await
            .unwrap();
        let sid = db.create_session("koda", dir.path()).await.unwrap();
        (db, dir, sid)
    }

    #[tokio::test]
    async fn test_recall_no_query_or_turn() {
        let (db, _dir, sid) = test_db().await;
        let result = recall_context(&db, &sid, &json!({})).await;
        assert!(
            result.contains("Provide"),
            "should ask for query or turn: {result}"
        );
    }

    #[tokio::test]
    async fn test_recall_empty_history() {
        let (db, _dir, sid) = test_db().await;
        let result = recall_context(&db, &sid, &json!({"turn": 1})).await;
        assert!(result.contains("No conversation history"), "got: {result}");
    }

    #[tokio::test]
    async fn test_recall_long_non_ascii_turn_does_not_panic() {
        let (db, _dir, sid) = test_db().await;
        let text = "界".repeat(700);
        db.insert_message(&sid, &Role::User, Some(&text), None, None, None)
            .await
            .unwrap();
        let result = recall_context(&db, &sid, &json!({"turn": 1})).await;
        assert!(result.contains("truncated"), "{result}");
    }

    #[tokio::test]
    async fn test_recall_by_turn_hit() {
        let (db, _dir, sid) = test_db().await;
        db.insert_message(&sid, &Role::User, Some("hello world"), None, None, None)
            .await
            .unwrap();
        let result = recall_context(&db, &sid, &json!({"turn": 1})).await;
        assert!(result.contains("hello world"), "got: {result}");
        assert!(result.contains("Turn 1"), "got: {result}");
    }

    #[tokio::test]
    async fn test_recall_by_turn_out_of_bounds() {
        let (db, _dir, sid) = test_db().await;
        db.insert_message(&sid, &Role::User, Some("msg1"), None, None, None)
            .await
            .unwrap();
        let result = recall_context(&db, &sid, &json!({"turn": 99})).await;
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[tokio::test]
    async fn test_recall_by_query_match() {
        let (db, _dir, sid) = test_db().await;
        db.insert_message(
            &sid,
            &Role::Assistant,
            Some("The error was a null pointer exception"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let result = recall_context(&db, &sid, &json!({"query": "null pointer"})).await;
        assert!(result.contains("null pointer"), "got: {result}");
        assert!(result.contains("Found"), "got: {result}");
    }

    #[tokio::test]
    async fn test_recall_by_query_no_match() {
        let (db, _dir, sid) = test_db().await;
        db.insert_message(&sid, &Role::User, Some("hello world"), None, None, None)
            .await
            .unwrap();
        let result = recall_context(&db, &sid, &json!({"query": "xyzzy"})).await;
        assert!(result.contains("No matches"), "got: {result}");
    }

    #[tokio::test]
    async fn test_recall_by_turn_long_content_truncated() {
        let (db, _dir, sid) = test_db().await;
        let long_msg = "z".repeat(3000);
        db.insert_message(&sid, &Role::User, Some(&long_msg), None, None, None)
            .await
            .unwrap();
        let result = recall_context(&db, &sid, &json!({"turn": 1})).await;
        assert!(
            result.contains("[truncated"),
            "long message should be truncated: {result}"
        );
    }
}
