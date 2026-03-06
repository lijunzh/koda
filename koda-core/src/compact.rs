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
    let prov = provider.read().await;
    let response = prov.chat(&messages, &[], model_settings).await?;

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
