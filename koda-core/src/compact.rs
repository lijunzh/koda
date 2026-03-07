//! Session compaction — summarize old messages to reclaim context.
//!
//! Pure logic, zero UI dependencies. Returns structured results
//! for the caller (TUI or headless) to render however it likes.

use crate::db::Database;
use crate::providers::{ChatMessage, LlmProvider};
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Number of recent messages to keep verbatim during compaction.
pub const COMPACT_PRESERVE_COUNT: usize = 4;

/// Result of a successful compaction.
#[derive(Debug)]
pub struct CompactResult {
    /// Number of messages deleted from the database.
    pub deleted: usize,
    /// Estimated tokens in the summary.
    pub summary_tokens: usize,
}

/// Why compaction was skipped (not an error, just a precondition).
#[derive(Debug)]
pub enum CompactSkip {
    PendingToolCalls,
    TooShort(usize),
}

/// Attempt to compact a session.
///
/// Returns `Ok(Ok(result))` on success, `Ok(Err(skip))` if a
/// precondition prevented compaction, or `Err(e)` on failure.
pub async fn compact_session(
    db: &Database,
    session_id: &str,
    max_context_tokens: usize,
    model_settings: &crate::config::ModelSettings,
    provider: &Arc<RwLock<Box<dyn LlmProvider>>>,
) -> Result<std::result::Result<CompactResult, CompactSkip>> {
    let prov = provider.read().await;
    compact_session_with_provider(db, session_id, max_context_tokens, model_settings, &**prov).await
}

/// Core compaction logic — accepts `&dyn LlmProvider` directly.
///
/// Used by the inference loop for pre-flight compaction (where we already
/// have a `&dyn LlmProvider` and don't need the Arc<RwLock<>> wrapper).
pub async fn compact_session_with_provider(
    db: &Database,
    session_id: &str,
    max_context_tokens: usize,
    model_settings: &crate::config::ModelSettings,
    provider: &dyn LlmProvider,
) -> Result<std::result::Result<CompactResult, CompactSkip>> {
    // Check preconditions
    if db.has_pending_tool_calls(session_id).await.unwrap_or(false) {
        return Ok(Err(CompactSkip::PendingToolCalls));
    }

    let history = db.load_context(session_id, max_context_tokens).await?;

    if history.len() < 4 {
        return Ok(Err(CompactSkip::TooShort(history.len())));
    }

    // Build conversation text for summarization
    let conversation_text = build_conversation_text(&history);

    let summary_prompt = format!(
        "Summarize the conversation below. This summary will replace the older messages \
         so an AI assistant can continue the session seamlessly.\n\
         \n\
         Preserve ALL of the following:\n\
         1. **User Intent** — Every goal, request, and requirement.\n\
         2. **Key Decisions** — Decisions made and their rationale.\n\
         3. **Files & Code** — Every file created, modified, or deleted.\n\
         4. **Errors & Fixes** — Bugs encountered and how they were resolved.\n\
         5. **Current State** — What is working, what has been tested.\n\
         6. **Pending Tasks** — Anything unfinished or deferred.\n\
         7. **Next Step** — Only if clearly stated or implied.\n\
         \n\
         Use concise bullet points. Do not add new ideas.\n\
         \n\
         ---\n\n{conversation_text}"
    );

    let messages = vec![ChatMessage::text("user", &summary_prompt)];
    let response = provider.chat(&messages, &[], model_settings).await?;

    let summary = match response.content {
        Some(text) if !text.trim().is_empty() => text,
        _ => bail!("LLM returned an empty summary"),
    };

    let compact_message = format!("[Compacted conversation summary]\n\n{summary}");
    let deleted = db
        .compact_session(session_id, &compact_message, COMPACT_PRESERVE_COUNT)
        .await?;

    Ok(Ok(CompactResult {
        deleted,
        summary_tokens: summary.len() / 4,
    }))
}

/// Format conversation history into a single string for the summarizer.
fn build_conversation_text(history: &[crate::db::Message]) -> String {
    let mut text = String::new();
    for msg in history {
        let role = msg.role.as_str();
        if let Some(ref content) = msg.content {
            let truncated: String = content.chars().take(2000).collect();
            text.push_str(&format!("[{role}]: {truncated}\n\n"));
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            let truncated: String = tool_calls.chars().take(500).collect();
            text.push_str(&format!("[{role} tool_calls]: {truncated}\n\n"));
        }
    }
    // Cap total text
    const MAX_TEXT: usize = 20_000;
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n\n[...truncated for summarization...]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Message;

    fn make_msg(role: &str, content: Option<&str>, tool_calls: Option<&str>) -> Message {
        Message {
            id: 0,
            session_id: String::new(),
            role: role.to_string(),
            content: content.map(String::from),
            tool_calls: tool_calls.map(String::from),
            tool_call_id: None,
            prompt_tokens: None,
            completion_tokens: None,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            thinking_tokens: None,
        }
    }

    #[test]
    fn test_empty_history() {
        assert_eq!(build_conversation_text(&[]), "");
    }

    #[test]
    fn test_basic_conversation() {
        let msgs = vec![
            make_msg("user", Some("hello"), None),
            make_msg("assistant", Some("hi"), None),
        ];
        let text = build_conversation_text(&msgs);
        assert!(text.contains("[user]: hello"));
        assert!(text.contains("[assistant]: hi"));
    }

    #[test]
    fn test_truncates_long_content_per_message() {
        let long = "x".repeat(3000);
        let msgs = vec![make_msg("user", Some(&long), None)];
        let text = build_conversation_text(&msgs);
        // Each msg content capped at 2000 chars
        assert!(text.len() < 2100);
    }

    #[test]
    fn test_truncates_total_over_20k() {
        // 50 messages × 500 chars each = 25K chars
        let content = "y".repeat(500);
        let msgs: Vec<_> = (0..50)
            .map(|_| make_msg("user", Some(&content), None))
            .collect();
        let text = build_conversation_text(&msgs);
        assert!(text.len() <= 20_100); // 20K + truncation message
        assert!(text.contains("[...truncated for summarization...]"));
    }

    #[test]
    fn test_multibyte_boundary_safe() {
        // Put emoji right at the 2000-char boundary
        let mut content = "a".repeat(1999);
        content.push('\u{1f43b}'); // bear emoji (4 bytes)
        content.push_str("after");
        let msgs = vec![make_msg("user", Some(&content), None)];
        let text = build_conversation_text(&msgs);
        // Should not panic on char boundary
        assert!(text.contains("\u{1f43b}") || !text.contains("after"));
    }

    #[test]
    fn test_tool_calls_included() {
        let msgs = vec![make_msg("assistant", None, Some("{\"name\": \"Read\"}"))];
        let text = build_conversation_text(&msgs);
        assert!(text.contains("tool_calls"));
        assert!(text.contains("Read"));
    }

    #[test]
    fn test_none_content_skipped() {
        let msgs = vec![make_msg("tool", None, None)];
        let text = build_conversation_text(&msgs);
        assert_eq!(text, "");
    }
}
