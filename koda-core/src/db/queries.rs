//! `Persistence` trait implementation for `Database`.
//!
//! All SQL queries that fulfill the `Persistence` contract live here,
//! along with the `prune_mismatched_tool_calls` helper.

use anyhow::Result;
use std::path::Path;

use super::{Database, MessageRow, SessionInfoRow};
use crate::persistence::{CompactedStats, Message, Persistence, Role, SessionInfo, SessionUsage};

// ── Private helpers ─────────────────────────────────────────────────────────

/// Remove messages with mismatched tool_use / tool_result pairing (#428).
///
/// Uses the symmetric-difference approach (inspired by Code Puppy):
/// collect all tool_call IDs from calls and returns, find IDs that appear
/// in only one set, and drop any message referencing a mismatched ID.
///
/// Handles orphans from any source: interrupted sessions, compaction
/// boundaries, or session resume.
pub(crate) fn prune_mismatched_tool_calls(messages: &mut Vec<Message>) {
    if messages.is_empty() {
        return;
    }

    let mut tool_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tool_return_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages.iter() {
        if msg.role == Role::Assistant {
            if let Some(ref tc_json) = msg.tool_calls
                && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
            {
                for call in &calls {
                    if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                        tool_call_ids.insert(id.to_string());
                    }
                }
            }
        } else if msg.role == Role::Tool
            && let Some(ref id) = msg.tool_call_id
        {
            tool_return_ids.insert(id.clone());
        }
    }

    let mismatched: std::collections::HashSet<&String> = tool_call_ids
        .symmetric_difference(&tool_return_ids)
        .collect();

    if mismatched.is_empty() {
        return;
    }

    messages.retain(|msg| {
        // Drop tool_result messages with mismatched IDs
        if msg.role == Role::Tool
            && let Some(ref id) = msg.tool_call_id
            && mismatched.contains(id)
        {
            return false;
        }
        // Drop assistant messages whose tool_calls contain mismatched IDs
        if msg.role == Role::Assistant
            && let Some(ref tc_json) = msg.tool_calls
            && let Ok(calls) = serde_json::from_str::<Vec<serde_json::Value>>(tc_json)
        {
            let has_mismatched = calls.iter().any(|call| {
                call.get("id")
                    .and_then(|v| v.as_str())
                    .is_some_and(|id| mismatched.contains(&id.to_string()))
            });
            if has_mismatched {
                return false;
            }
        }
        true
    });
}

/// Drop assistant messages that have neither text content nor tool calls.
///
/// These are "ghost" messages: the stream was interrupted (network drop or
/// Ctrl+C) before the model produced any output — only thinking deltas
/// arrived, which are never persisted. Sending such a message to the
/// provider on resume triggers `invalid_request_error` (all-null content).
///
/// Note: this situation should no longer arise after the `NetworkError`
/// detection fix in #594, but the prune pass acts as a safety net for
/// any messages that slipped through before the fix was deployed.
pub(crate) fn prune_null_content_messages(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        if msg.role != Role::Assistant {
            return true; // keep non-assistant messages unchanged
        }
        let has_content = msg.content.as_deref().is_some_and(|c| !c.trim().is_empty());
        let has_tool_calls = msg.tool_calls.is_some();
        has_content || has_tool_calls
    });
}

/// Drop assistant messages whose text content is entirely whitespace.
///
/// These appear when the connection is lost just after the model emits a
/// `\n\n` prefix (common before a `<think>` block). The resulting message
/// is harmless but adds noise to the conversation and can confuse the model
/// into thinking it already replied to the previous user turn.
pub(crate) fn prune_whitespace_only_messages(messages: &mut Vec<Message>) {
    messages.retain(|msg| {
        if msg.role != Role::Assistant {
            return true;
        }
        // Keep if there are tool calls regardless of content.
        if msg.tool_calls.is_some() {
            return true;
        }
        // Drop if content is present but whitespace-only.
        !matches!(msg.content.as_deref(), Some(c) if c.trim().is_empty())
    });
}

// ── Persistence trait ───────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Persistence for Database {
    /// Create a new session, returning the generated session ID.
    async fn create_session(&self, agent_name: &str, project_root: &Path) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let root = project_root.to_string_lossy().to_string();
        sqlx::query("INSERT INTO sessions (id, agent_name, project_root) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(agent_name)
            .bind(&root)
            .execute(&self.pool)
            .await?;
        tracing::info!("Created session: {id} (project: {root})");
        Ok(id)
    }

    /// Insert a message into the conversation log.
    async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::providers::TokenUsage>,
    ) -> Result<i64> {
        self.insert_message_with_agent(
            session_id,
            role,
            content,
            tool_calls,
            tool_call_id,
            usage,
            None,
        )
        .await
    }

    /// Insert a message with an optional agent name for cost tracking.
    #[allow(clippy::too_many_arguments)]
    async fn insert_message_with_agent(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::providers::TokenUsage>,
        agent_name: Option<&str>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, \
             prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens, \
             thinking_tokens, agent_name)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(role.as_str())
        .bind(content)
        .bind(tool_calls)
        .bind(tool_call_id)
        .bind(usage.map(|u| u.prompt_tokens))
        .bind(usage.map(|u| u.completion_tokens))
        .bind(usage.map(|u| u.cache_read_tokens))
        .bind(usage.map(|u| u.cache_creation_tokens))
        .bind(usage.map(|u| u.thinking_tokens))
        .bind(agent_name)
        .execute(&self.pool)
        .await?;

        // Update session activity timestamp
        sqlx::query("UPDATE sessions SET last_accessed_at = datetime('now') WHERE id = ?")
            .bind(session_id)
            .execute(&self.pool)
            .await?;

        Ok(result.last_insert_rowid())
    }

    async fn mark_message_complete(&self, message_id: i64) -> Result<()> {
        sqlx::query("UPDATE messages SET completed_at = datetime('now') WHERE id = ?")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load active (non-compacted) messages for a session.
    ///
    /// Returns messages in chronological order. Compacted messages
    /// (archived by `/compact`) are excluded — their summary replaces them.
    /// Several sanitisation passes run before returning:
    /// - Mismatched tool_use / tool_result pairs are pruned (#428)
    /// - Null-content assistant messages with no tool calls are dropped (#594)
    /// - Whitespace-only assistant messages are dropped (#594)
    async fn load_context(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut messages: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, role, content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens
             FROM messages
             WHERE session_id = ? AND compacted_at IS NULL
             ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| r.into())
        .collect();

        // Sanitisation passes: run in order, each sees the output of the previous.
        prune_mismatched_tool_calls(&mut messages); // (#428)
        prune_null_content_messages(&mut messages); // (#594)
        prune_whitespace_only_messages(&mut messages); // (#594)

        Ok(messages)
    }

    /// Load ALL messages for a session (for RecallContext search).
    /// Returns messages in chronological order, no truncation.
    async fn load_all_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, role, content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens
             FROM messages
             WHERE session_id = ?
             ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| r.into())
        .collect();
        Ok(rows)
    }

    /// Load recent user messages across all sessions (for the startup banner).
    /// Returns up to `limit` messages, newest first.
    async fn recent_user_messages(&self, limit: i64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT content FROM messages
             WHERE role = 'user' AND content IS NOT NULL AND content != ''
             ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Get token usage totals for a session.
    async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage> {
        let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(thinking_tokens), 0),
                COUNT(*)
             FROM messages
             WHERE session_id = ?
               AND (prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL)",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(SessionUsage {
            prompt_tokens: row.0,
            completion_tokens: row.1,
            cache_read_tokens: row.2,
            cache_creation_tokens: row.3,
            thinking_tokens: row.4,
            api_calls: row.5,
        })
    }

    /// Get token usage broken down by agent name.
    async fn session_usage_by_agent(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, SessionUsage)>> {
        let rows: Vec<(String, i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT
                COALESCE(agent_name, 'main'),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_creation_tokens), 0),
                COALESCE(SUM(thinking_tokens), 0),
                COUNT(*)
             FROM messages
             WHERE session_id = ?
               AND (prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL)
             GROUP BY COALESCE(agent_name, 'main')
             ORDER BY COALESCE(SUM(prompt_tokens), 0) + COALESCE(SUM(completion_tokens), 0) DESC",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.0,
                    SessionUsage {
                        prompt_tokens: r.1,
                        completion_tokens: r.2,
                        cache_read_tokens: r.3,
                        cache_creation_tokens: r.4,
                        thinking_tokens: r.5,
                        api_calls: r.6,
                    },
                )
            })
            .collect())
    }

    /// List recent sessions for a specific project.
    async fn list_sessions(&self, limit: i64, project_root: &Path) -> Result<Vec<SessionInfo>> {
        let root = project_root.to_string_lossy().to_string();
        let rows: Vec<SessionInfoRow> = sqlx::query_as(
            "SELECT s.id, s.agent_name, s.created_at,
                    COUNT(m.id) as message_count,
                    COALESCE(SUM(m.prompt_tokens), 0) + COALESCE(SUM(m.completion_tokens), 0) as total_tokens
             FROM sessions s
             LEFT JOIN messages m ON m.session_id = s.id
             WHERE s.project_root = ? OR s.project_root IS NULL
             GROUP BY s.id
             ORDER BY s.created_at DESC, s.rowid DESC
             LIMIT ?",
        )
        .bind(&root)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Get the last assistant text response for a session (for headless JSON output).
    async fn last_assistant_message(&self, session_id: &str) -> Result<String> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT content FROM messages
             WHERE session_id = ? AND role = 'assistant' AND content IS NOT NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0).unwrap_or_default())
    }

    /// Get the last user message in a session.
    async fn last_user_message(&self, session_id: &str) -> Result<String> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT content FROM messages
             WHERE session_id = ? AND role = 'user' AND content IS NOT NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0).unwrap_or_default())
    }

    /// Delete a session and all its messages/metadata atomically.
    async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        sqlx::query("DELETE FROM session_metadata WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        // Reclaim freed pages from the deletion.
        sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Compact a session: summarize old messages while preserving the most recent ones.
    ///
    /// Keeps the last `preserve_count` messages intact, deletes the rest, and
    /// inserts a summary (as a `system` message) plus a continuation hint
    /// (as an `assistant` message) before the preserved tail.
    ///
    /// Returns the number of messages that were deleted/replaced.
    async fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        preserve_count: usize,
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await?;

        // Get active (non-compacted) message IDs ordered oldest→newest
        let all_ids: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE session_id = ? AND compacted_at IS NULL ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;

        let total = all_ids.len();
        if total == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Determine which messages to archive (everything except the tail)
        let keep_from = total.saturating_sub(preserve_count);
        let ids_to_archive: Vec<i64> = all_ids[..keep_from].iter().map(|r| r.0).collect();
        let archived_count = ids_to_archive.len();

        if archived_count == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Mark old messages as compacted (non-destructive — history preserved in DB)
        for chunk in ids_to_archive.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE messages SET compacted_at = datetime('now') \
                 WHERE session_id = ? AND id IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql).bind(session_id);
            for id in chunk {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await?;
        }

        // Insert the summary as a system message
        sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, prompt_tokens, completion_tokens)
             VALUES (?, 'system', ?, NULL, NULL, NULL, NULL)",
        )
        .bind(session_id)
        .bind(summary)
        .execute(&mut *tx)
        .await?;

        // Insert a continuation hint so the LLM knows how to behave
        let continuation = "Your context was compacted. The previous message contains a summary of our earlier conversation. \
            Do not mention the summary or that compaction occurred. \
            Continue the conversation naturally based on the summarized context.";
        sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, prompt_tokens, completion_tokens)
             VALUES (?, 'assistant', ?, NULL, NULL, NULL, NULL)",
        )
        .bind(session_id)
        .bind(continuation)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(archived_count)
    }

    /// Check if the last message in a session is a tool call awaiting a response.
    /// Used to defer compaction during active tool execution.
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool> {
        let last_msg: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT role, tool_calls FROM messages
             WHERE session_id = ? AND compacted_at IS NULL
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(matches!(last_msg, Some((role, Some(_))) if role == "assistant"))
    }

    /// Stats about compacted (archived) messages across all sessions.
    async fn compacted_stats(&self) -> Result<CompactedStats> {
        let row: (i64, i64, i64, Option<String>) = sqlx::query_as(
            "SELECT
                 COUNT(*),
                 COUNT(DISTINCT session_id),
                 COALESCE(SUM(LENGTH(content) + LENGTH(COALESCE(tool_calls,''))), 0),
                 MIN(compacted_at)
             FROM messages
             WHERE compacted_at IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(CompactedStats {
            message_count: row.0,
            session_count: row.1,
            size_bytes: row.2,
            oldest: row.3,
        })
    }

    /// Permanently delete compacted messages older than `min_age_days`.
    /// Pass 0 to delete all compacted messages regardless of age.
    async fn purge_compacted(&self, min_age_days: u32) -> Result<usize> {
        let result = if min_age_days == 0 {
            sqlx::query("DELETE FROM messages WHERE compacted_at IS NOT NULL")
                .execute(&self.pool)
                .await?
        } else {
            sqlx::query(
                "DELETE FROM messages
                 WHERE compacted_at IS NOT NULL
                   AND compacted_at < datetime('now', ?)",
            )
            .bind(format!("-{min_age_days} days"))
            .execute(&self.pool)
            .await?
        };

        let deleted = result.rows_affected() as usize;

        // Reclaim disk space.
        sqlx::query("VACUUM").execute(&self.pool).await?;

        tracing::info!("Purged {deleted} compacted messages (>{min_age_days} days old)");
        Ok(deleted)
    }

    /// Get a session metadata value by key.
    async fn get_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM session_metadata WHERE session_id = ? AND key = ?")
                .bind(session_id)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }

    /// Set a session metadata value (upsert).
    async fn set_metadata(&self, session_id: &str, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO session_metadata (session_id, key, value, updated_at)
             VALUES (?, ?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(session_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(session_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get the todo list for a session (convenience wrapper).
    async fn get_todo(&self, session_id: &str) -> Result<Option<String>> {
        self.get_metadata(session_id, "todo").await
    }

    /// Set the todo list for a session (convenience wrapper).
    async fn set_todo(&self, session_id: &str, content: &str) -> Result<()> {
        self.set_metadata(session_id, "todo", content).await
    }
}
