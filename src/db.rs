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
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, prompt_tokens, completion_tokens)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(role.as_str())
        .bind(content)
        .bind(tool_calls)
        .bind(tool_call_id)
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .execute(&self.pool)
        .await?;

        Ok(result.last_insert_rowid())
    }

    /// Load recent messages for a session, applying a sliding window.
    /// Returns messages newest-first, capped at `max_tokens` estimated usage.
    pub async fn load_context(&self, session_id: &str, max_tokens: usize) -> Result<Vec<Message>> {
        let rows: Vec<Message> = sqlx::query_as::<_, MessageRow>(
            "SELECT id, session_id, role, content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens
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
    pub async fn session_token_usage(&self, session_id: &str) -> Result<(i64, i64, i64)> {
        let row: (i64, i64, i64) = sqlx::query_as(
            "SELECT
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COUNT(*)
             FROM messages
             WHERE session_id = ?
               AND (prompt_tokens IS NOT NULL OR completion_tokens IS NOT NULL)",
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
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

    /// Replace all messages in a session with a single summary message.
    /// Used by `/compact` to reclaim context window space.
    pub async fn compact_session(&self, session_id: &str, summary: &str) -> Result<usize> {
        let mut tx = self.pool.begin().await?;

        // Count existing messages for reporting
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;

        // Delete all existing messages
        sqlx::query("DELETE FROM messages WHERE session_id = ?")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;

        // Insert the summary as a user message so the LLM has context
        sqlx::query(
            "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, prompt_tokens, completion_tokens)
             VALUES (?, 'user', ?, NULL, NULL, NULL, NULL)",
        )
        .bind(session_id)
        .bind(summary)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(count as usize)
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

        db.insert_message(&session, &Role::User, Some("hello"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("hi there!"),
            None,
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
            db.insert_message(
                &session,
                &Role::User,
                Some(&content),
                None,
                None,
                None,
                None,
            )
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

        db.insert_message(&s1, &Role::User, Some("session 1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(&s2, &Role::User, Some("session 2"), None, None, None, None)
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

        db.insert_message(&session, &Role::User, Some("q1"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("a1"),
            None,
            None,
            Some(100),
            Some(50),
        )
        .await
        .unwrap();
        db.insert_message(&session, &Role::User, Some("q2"), None, None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("a2"),
            None,
            None,
            Some(200),
            Some(80),
        )
        .await
        .unwrap();

        let (prompt, completion, turns) = db.session_token_usage(&session).await.unwrap();
        assert_eq!(prompt, 300);
        assert_eq!(completion, 130);
        assert_eq!(turns, 2);
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
        db.insert_message(&s1, &Role::User, Some("hello"), None, None, None, None)
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
            let role = if i % 2 == 0 { &Role::User } else { &Role::Assistant };
            db.insert_message(&session, role, Some(&format!("msg {i}")), None, None, None, None)
                .await
                .unwrap();
        }

        let deleted = db.compact_session(&session, "Summary of conversation").await.unwrap();
        assert_eq!(deleted, 10);

        // Should have exactly 1 message now
        let msgs = db.load_context(&session, 100_000).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].content.as_ref().unwrap().contains("Summary of conversation"));
    }

    #[tokio::test]
    async fn test_token_usage_empty_session() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default").await.unwrap();

        let (prompt, completion, turns) = db.session_token_usage(&session).await.unwrap();
        assert_eq!(prompt, 0);
        assert_eq!(completion, 0);
        assert_eq!(turns, 0);
    }
}
