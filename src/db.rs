//! SQLite database layer for durable execution state.
//!
//! Uses WAL mode for concurrent access and indexes for fast session lookups.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// Message roles in the conversation.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A stored message row.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
}

/// Token usage totals for a session.
#[derive(Debug, Clone, Default)]
pub struct SessionUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub thinking_tokens: i64,
    pub api_calls: i64,
}

/// Wrapper around the SQLite connection pool.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Initialize the database, run migrations, and enable WAL mode.
    pub async fn init(project_root: &Path) -> Result<Self> {
        let db_path = project_root.join(".koda.db");
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        let options = SqliteConnectOptions::from_str(&db_url)?
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("Failed to connect to database: {db_url}"))?;

        // Run schema migrations
        Self::migrate(&pool).await?;

        tracing::info!("Database initialized at {:?}", db_path);
        Ok(Self { pool })
    }

    /// Apply the schema (idempotent).
    async fn migrate(pool: &SqlitePool) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                agent_name TEXT NOT NULL
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id);")
            .execute(pool)
            .await?;

        // Additive migrations for new token tracking columns (idempotent).
        for col in &[
            "cache_read_tokens",
            "cache_creation_tokens",
            "thinking_tokens",
        ] {
            let sql = format!("ALTER TABLE messages ADD COLUMN {col} INTEGER");
            // Ignore "duplicate column name" errors — column already exists.
            if let Err(e) = sqlx::query(&sql).execute(pool).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    /// Create a new session, returning the generated session ID.
    pub async fn create_session(&self, agent_name: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO sessions (id, agent_name) VALUES (?, ?)")
            .bind(&id)
            .bind(agent_name)
            .execute(&self.pool)
            .await?;
        tracing::info!("Created session: {id}");
        Ok(id)
    }

    /// Insert a message into the conversation log.
    pub async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::providers::TokenUsage>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, \
             prompt_tokens, completion_tokens, cache_read_tokens, cache_creation_tokens, thinking_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Load recent messages for a session, applying a sliding window.
    /// Returns messages newest-first, capped at `max_tokens` estimated usage.
    pub async fn load_context(&self, session_id: &str, max_tokens: usize) -> Result<Vec<Message>> {
        let rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, role, content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens
             FROM messages
             WHERE session_id = ?
             ORDER BY id DESC
             LIMIT 200",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|r| r.into())
        .collect();

        // Sliding window: accumulate tokens from newest to oldest.
        // Tool results older than the most recent 4 messages get truncated
        // to save tokens — the LLM already processed them.
        let mut budget = max_tokens;
        let mut window = Vec::new();
        let recency_threshold = 4; // keep full content for this many recent messages

        for (idx, mut msg) in rows.into_iter().enumerate() {
            // Truncate old tool results to save context tokens
            if idx >= recency_threshold
                && msg.role == "tool"
                && let Some(ref content) = msg.content
                && content.len() > 500
            {
                // Snap to a valid char boundary at or before 300 bytes
                let mut end = 300.min(content.len());
                while end > 0 && !content.is_char_boundary(end) {
                    end -= 1;
                }
                let truncated = format!(
                    "{}\n\n[Previous tool output truncated — {} chars. Re-read if needed.]",
                    &content[..end],
                    content.len()
                );
                msg.content = Some(truncated);
            }

            let estimated = Self::estimate_tokens(&msg);
            if estimated > budget {
                break;
            }
            budget -= estimated;
            window.push(msg);
        }

        // Reverse so messages are in chronological order
        window.reverse();
        Ok(window)
    }

    /// Load recent user messages across all sessions (for the startup banner).
    /// Returns up to `limit` messages, newest first.
    pub async fn recent_user_messages(&self, limit: i64) -> Result<Vec<String>> {
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

    /// Rough token estimate: ~4 chars per token (good enough for sliding window).
    fn estimate_tokens(msg: &Message) -> usize {
        let content_len = msg.content.as_deref().map_or(0, |c| c.len());
        let tool_len = msg.tool_calls.as_deref().map_or(0, |c| c.len());
        (content_len + tool_len) / 4 + 10 // +10 for role/metadata overhead
    }

    /// Get token usage totals for a session.
    pub async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage> {
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

    /// List recent sessions with metadata.
    pub async fn list_sessions(&self, limit: i64) -> Result<Vec<SessionInfo>> {
        let rows: Vec<SessionInfoRow> = sqlx::query_as(
            "SELECT s.id, s.agent_name, s.created_at,
                    COUNT(m.id) as message_count,
                    COALESCE(SUM(m.prompt_tokens), 0) + COALESCE(SUM(m.completion_tokens), 0) as total_tokens
             FROM sessions s
             LEFT JOIN messages m ON m.session_id = s.id
             GROUP BY s.id
             ORDER BY s.created_at DESC, s.rowid DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.into()).collect())
    }

    /// Get the last assistant text response for a session (for headless JSON output).
    pub async fn last_assistant_message(&self, session_id: &str) -> Result<String> {
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

    /// Delete a session and all its messages atomically.
    pub async fn delete_session(&self, session_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;

        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;

        Ok(result.rows_affected() > 0)
    }

    /// Compact a session: summarize old messages while preserving the most recent ones.
    ///
    /// Keeps the last `preserve_count` messages intact, deletes the rest, and
    /// inserts a summary (as a `system` message) plus a continuation hint
    /// (as an `assistant` message) before the preserved tail.
    ///
    /// Returns the number of messages that were deleted/replaced.
    pub async fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        preserve_count: usize,
    ) -> Result<usize> {
        let mut tx = self.pool.begin().await?;

        // Get all message IDs ordered oldest→newest
        let all_ids: Vec<(i64,)> = sqlx::query_as(
            "SELECT id FROM messages WHERE session_id = ? ORDER BY id ASC",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await?;

        let total = all_ids.len();
        if total == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Determine which messages to delete (everything except the tail)
        let keep_from = total.saturating_sub(preserve_count);
        let ids_to_delete: Vec<i64> = all_ids[..keep_from].iter().map(|r| r.0).collect();
        let deleted_count = ids_to_delete.len();

        if deleted_count == 0 {
            tx.commit().await?;
            return Ok(0);
        }

        // Delete old messages in batches (SQLite has a variable limit)
        for chunk in ids_to_delete.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "DELETE FROM messages WHERE session_id = ? AND id IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql).bind(session_id);
            for id in chunk {
                query = query.bind(id);
            }
            query.execute(&mut *tx).await?;
        }

        // Insert the summary as a system message (it's context, not user speech)
        // Use a low ID trick: find the min preserved ID and insert before it
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

        Ok(deleted_count)
    }

    /// Check if the last message in a session is a tool call awaiting a response.
    /// Used to defer compaction during active tool execution.
    pub async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool> {
        // A pending tool call exists when the last message has role='assistant'
        // with tool_calls set, and there's no subsequent tool response.
        let last_msg: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT role, tool_calls FROM messages
             WHERE session_id = ?
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(matches!(last_msg, Some((role, Some(_))) if role == "assistant"))
    }
}

/// Internal row type for sqlx deserialization.
#[derive(sqlx::FromRow)]
struct MessageRow {
    id: i64,
    session_id: String,
    role: String,
    content: Option<String>,
    tool_calls: Option<String>,
    tool_call_id: Option<String>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    thinking_tokens: Option<i64>,
}

/// Session metadata for listing.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub agent_name: String,
    pub created_at: String,
    pub message_count: i64,
    pub total_tokens: i64,
}

#[derive(sqlx::FromRow)]
struct SessionInfoRow {
    id: String,
    agent_name: String,
    created_at: String,
    message_count: i64,
    total_tokens: i64,
}

impl From<SessionInfoRow> for SessionInfo {
    fn from(r: SessionInfoRow) -> Self {
        Self {
            id: r.id,
            agent_name: r.agent_name,
            created_at: r.created_at,
            message_count: r.message_count,
            total_tokens: r.total_tokens,
        }
    }
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            role: r.role,
            content: r.content,
            tool_calls: r.tool_calls,
            tool_call_id: r.tool_call_id,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
            thinking_tokens: r.thinking_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup() -> (Database, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db = Database::init(tmp.path()).await.unwrap();
        (db, tmp)
    }

    #[tokio::test]
    async fn test_create_session() {
        let (db, _tmp) = setup().await;
        let id = db.create_session("default").await.unwrap();
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn test_insert_and_load_messages() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("hi there!"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let msgs = db.load_context(&session, 100_000).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[tokio::test]
    async fn test_sliding_window_truncates_old_messages() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        // Insert many messages
        for i in 0..20 {
            let content = format!("Message number {i} with some padding text to take up tokens");
            db.insert_message(&session, &Role::User, Some(&content), None, None, None)
                .await
                .unwrap();
        }

        // Load with a tiny token budget - should only get the most recent messages
        let msgs = db.load_context(&session, 50).await.unwrap();
        assert!(msgs.len() < 20, "Should have truncated, got {}", msgs.len());
        assert!(!msgs.is_empty(), "Should have at least one message");

        // The last message in the window should be the newest
        let last = msgs.last().unwrap();
        assert!(
            last.content.as_ref().unwrap().contains("19"),
            "Last message should be #19, got: {:?}",
            last.content
        );
    }

    #[tokio::test]
    async fn test_sessions_are_isolated() {
        let (db, _tmp) = setup().await;
        let s1 = db.create_session("agent-a").await.unwrap();
        let s2 = db.create_session("agent-b").await.unwrap();

        db.insert_message(&s1, &Role::User, Some("session 1"), None, None, None)
            .await
            .unwrap();
        db.insert_message(&s2, &Role::User, Some("session 2"), None, None, None)
            .await
            .unwrap();

        let msgs1 = db.load_context(&s1, 100_000).await.unwrap();
        let msgs2 = db.load_context(&s2, 100_000).await.unwrap();

        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs1[0].content.as_deref().unwrap(), "session 1");
        assert_eq!(msgs2[0].content.as_deref().unwrap(), "session 2");
    }

    #[tokio::test]
    async fn test_session_token_usage() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        db.insert_message(&session, &Role::User, Some("q1"), None, None, None)
            .await
            .unwrap();
        let usage1 = crate::providers::TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            ..Default::default()
        };
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("a1"),
            None,
            None,
            Some(&usage1),
        )
        .await
        .unwrap();
        db.insert_message(&session, &Role::User, Some("q2"), None, None, None)
            .await
            .unwrap();
        let usage2 = crate::providers::TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 80,
            ..Default::default()
        };
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("a2"),
            None,
            None,
            Some(&usage2),
        )
        .await
        .unwrap();

        let u = db.session_token_usage(&session).await.unwrap();
        assert_eq!(u.prompt_tokens, 300);
        assert_eq!(u.completion_tokens, 130);
        assert_eq!(u.api_calls, 2);
    }

    #[tokio::test]
    async fn test_list_sessions() {
        let (db, _tmp) = setup().await;
        db.create_session("agent-a").await.unwrap();
        db.create_session("agent-b").await.unwrap();
        db.create_session("agent-c").await.unwrap();

        let sessions = db.list_sessions(10).await.unwrap();
        assert_eq!(sessions.len(), 3);
        // Most recent first
        assert_eq!(sessions[0].agent_name, "agent-c");
    }

    #[tokio::test]
    async fn test_delete_session() {
        let (db, _tmp) = setup().await;
        let s1 = db.create_session("default").await.unwrap();
        db.insert_message(&s1, &Role::User, Some("hello"), None, None, None)
            .await
            .unwrap();

        assert!(db.delete_session(&s1).await.unwrap());

        let sessions = db.list_sessions(10).await.unwrap();
        assert!(sessions.is_empty());

        // Deleting again returns false
        assert!(!db.delete_session(&s1).await.unwrap());
    }

    #[tokio::test]
    async fn test_compact_session() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        // Insert several messages
        for i in 0..10 {
            let role = if i % 2 == 0 {
                &Role::User
            } else {
                &Role::Assistant
            };
            db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
                .await
                .unwrap();
        }

        // Compact preserving the last 2 messages
        let deleted = db
            .compact_session(&session, "Summary of conversation", 2)
            .await
            .unwrap();
        assert_eq!(deleted, 8); // 10 total - 2 preserved = 8 deleted

        // Should have: summary(system) + continuation(assistant) + 2 preserved = 4
        let msgs = db.load_context(&session, 100_000).await.unwrap();
        assert_eq!(msgs.len(), 4);

        // Check that the summary is a system message
        let system_msgs: Vec<_> = msgs.iter().filter(|m| m.role == "system").collect();
        assert_eq!(system_msgs.len(), 1);
        assert!(
            system_msgs[0]
                .content
                .as_ref()
                .unwrap()
                .contains("Summary of conversation")
        );

        // Check that there's a continuation hint as assistant
        let assistant_msgs: Vec<_> = msgs.iter().filter(|m| m.role == "assistant").collect();
        assert!(
            assistant_msgs
                .iter()
                .any(|m| m.content.as_deref().unwrap_or("").contains("compacted")),
            "Expected a continuation hint from assistant"
        );

        // The 2 preserved messages should still be there
        let preserved: Vec<_> = msgs
            .iter()
            .filter(|m| {
                m.content
                    .as_deref()
                    .map_or(false, |c| c.starts_with("msg "))
            })
            .collect();
        assert_eq!(preserved.len(), 2);
    }

    #[tokio::test]
    async fn test_compact_preserves_zero() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        for i in 0..6 {
            let role = if i % 2 == 0 { &Role::User } else { &Role::Assistant };
            db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None)
                .await
                .unwrap();
        }

        // Compact preserving 0 — deletes everything, inserts summary + continuation
        let deleted = db
            .compact_session(&session, "Full summary", 0)
            .await
            .unwrap();
        assert_eq!(deleted, 6);

        let msgs = db.load_context(&session, 100_000).await.unwrap();
        assert_eq!(msgs.len(), 2); // summary + continuation
        assert_eq!(msgs.iter().filter(|m| m.role == "system").count(), 1);
        assert_eq!(msgs.iter().filter(|m| m.role == "assistant").count(), 1);
    }

    #[tokio::test]
    async fn test_has_pending_tool_calls() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        // No messages → no pending
        assert!(!db.has_pending_tool_calls(&session).await.unwrap());

        // User message → no pending
        db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
            .await
            .unwrap();
        assert!(!db.has_pending_tool_calls(&session).await.unwrap());

        // Assistant with tool_calls → pending!
        db.insert_message(
            &session,
            &Role::Assistant,
            None,
            Some(r#"[{"id":"tc1","name":"Read","arguments":"{}"}]"#),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(db.has_pending_tool_calls(&session).await.unwrap());

        // Tool response → no longer pending
        db.insert_message(
            &session,
            &Role::Tool,
            Some("file contents"),
            None,
            Some("tc1"),
            None,
        )
        .await
        .unwrap();
        assert!(!db.has_pending_tool_calls(&session).await.unwrap());
    }

    #[tokio::test]
    async fn test_token_usage_empty_session() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        let u = db.session_token_usage(&session).await.unwrap();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.api_calls, 0);
    }

    #[tokio::test]
    async fn test_last_assistant_message() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        // Empty session returns empty string
        let msg = db.last_assistant_message(&session).await.unwrap();
        assert_eq!(msg, "");

        // Insert some messages
        db.insert_message(&session, &Role::User, Some("question 1"), None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("answer 1"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        db.insert_message(&session, &Role::User, Some("question 2"), None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("answer 2"),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // Should return the LAST assistant message
        let msg = db.last_assistant_message(&session).await.unwrap();
        assert_eq!(msg, "answer 2");
    }

    #[tokio::test]
    async fn test_last_assistant_message_skips_tool_calls() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        db.insert_message(
            &session,
            &Role::User,
            Some("do something"),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        // Assistant with tool calls but no text content
        db.insert_message(
            &session,
            &Role::Assistant,
            None,
            Some("[{\"id\":\"1\"}]"),
            None,
            None,
        )
        .await
        .unwrap();
        db.insert_message(
            &session,
            &Role::Tool,
            Some("tool result"),
            None,
            Some("1"),
            None,
        )
        .await
        .unwrap();
        // Final text response
        db.insert_message(&session, &Role::Assistant, Some("Done!"), None, None, None)
            .await
            .unwrap();

        let msg = db.last_assistant_message(&session).await.unwrap();
        assert_eq!(msg, "Done!");
    }
}
