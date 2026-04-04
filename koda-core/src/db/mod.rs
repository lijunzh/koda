//! SQLite persistence layer.
//!
//! Implements `Persistence` trait for SQLite via sqlx.
//! Uses WAL mode for concurrent access.
//!
//! ## Module layout
//!
//! - **mod.rs** — `Database` struct, init/open, schema migrations, row types
//! - **queries.rs** — `Persistence` trait implementation (all SQL queries)

pub mod queries;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// Re-export persistence types for backward compatibility.
pub use crate::persistence::{
    CompactedStats, Message, Persistence, Role, SessionInfo, SessionUsage,
};

/// Wrapper around the SQLite connection pool.
#[derive(Debug, Clone)]
pub struct Database {
    pub(crate) pool: SqlitePool,
}

/// Get the koda config directory (~/.config/koda/).
pub fn config_dir() -> Result<std::path::PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Cannot determine config directory (set HOME or XDG_CONFIG_HOME)")
        })?;
    Ok(base.join("koda"))
}

impl Database {
    /// Access the underlying connection pool (for tests and raw queries).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Initialize the database, run migrations, and enable WAL mode.
    ///
    /// `koda_config_dir` is the koda configuration directory (e.g. `~/.config/koda`).
    /// The database lives in `<koda_config_dir>/db/koda.db`.
    ///
    /// Production callers should pass `db::config_dir()?`; tests pass a temp dir.
    pub async fn init(koda_config_dir: &Path) -> Result<Self> {
        let db_dir = koda_config_dir.join("db");
        std::fs::create_dir_all(&db_dir)
            .with_context(|| format!("Failed to create DB dir: {}", db_dir.display()))?;

        let db_path = db_dir.join("koda.db");

        Self::open(&db_path).await
    }

    /// Open a database at a specific path (used by tests and init).
    pub async fn open(db_path: &Path) -> Result<Self> {
        let db_url = format!("sqlite:{}?mode=rwc", db_path.display());

        let options = SqliteConnectOptions::from_str(&db_url)?
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
            .foreign_keys(true)
            .create_if_missing(true)
            // Retry for up to 5 s when another connection holds the write
            // lock. Without this, concurrent writes from parallel sub-agents
            // (#595) return SQLITE_BUSY immediately and the insert is silently
            // dropped. Individual writes are ~1 ms so the retry resolves fast.
            .busy_timeout(std::time::Duration::from_millis(5000));

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("Failed to connect to database: {db_url}"))?;

        // Run schema migrations
        Self::migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Apply the schema (idempotent).
    async fn migrate(pool: &SqlitePool) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                agent_name TEXT NOT NULL,
                project_root TEXT,
                last_accessed_at TEXT,
                title TEXT,
                mode TEXT
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
                full_content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                prompt_tokens INTEGER,
                completion_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_creation_tokens INTEGER,
                thinking_tokens INTEGER,
                agent_name TEXT,
                compacted_at TEXT,
                completed_at DATETIME,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id);")
            .execute(pool)
            .await?;

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_role_id ON messages(role, id DESC);")
            .execute(pool)
            .await?;

        // Session-scoped key-value metadata (e.g. todo list).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS session_metadata (
                session_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(session_id, key),
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(pool)
        .await?;

        // File lifecycle tracking (#465): files created by Koda in a session.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS owned_files (
                session_id TEXT NOT NULL,
                path TEXT NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY(session_id, path)
            );",
        )
        .execute(pool)
        .await?;

        Ok(())
    }
}

// ── File lifecycle tracking (#465) ────────────────────────────────────────────

impl Database {
    /// Record that Koda created a file in this session.
    pub async fn insert_owned_file(&self, session_id: &str, path: &Path) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO owned_files (session_id, path) VALUES (?, ?)")
            .bind(session_id)
            .bind(path.to_string_lossy().as_ref())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Remove a file from the owned set.
    pub async fn delete_owned_file(&self, session_id: &str, path: &Path) -> Result<()> {
        sqlx::query("DELETE FROM owned_files WHERE session_id = ? AND path = ?")
            .bind(session_id)
            .bind(path.to_string_lossy().as_ref())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load all owned file paths for a session (used on session resume).
    pub async fn load_owned_files(
        &self,
        session_id: &str,
    ) -> Result<std::collections::HashSet<std::path::PathBuf>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT path FROM owned_files WHERE session_id = ?")
                .bind(session_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| std::path::PathBuf::from(p))
            .collect())
    }

    /// Load a page of messages older than `before_id` (for virtual scroll).
    ///
    /// Returns up to `limit` messages with `id < before_id`, ordered
    /// newest-first so the caller can reverse them for display.
    /// Only non-compacted messages are returned.
    pub async fn load_messages_before(
        &self,
        session_id: &str,
        before_id: i64,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, session_id, role, content, full_content, tool_calls, tool_call_id,
                    prompt_tokens, completion_tokens,
                    cache_read_tokens, cache_creation_tokens, thinking_tokens,
                    created_at
             FROM messages
             WHERE session_id = ? AND id < ? AND compacted_at IS NULL
             ORDER BY id DESC
             LIMIT ?",
        )
        .bind(session_id)
        .bind(before_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        // Reverse to chronological order
        let mut messages: Vec<Message> = rows.into_iter().map(|r| r.into()).collect();
        messages.reverse();
        Ok(messages)
    }

    /// Seconds since the last assistant message in this session.
    ///
    /// Returns `None` if there are no (non-compacted) assistant messages.
    /// Used by microcompact to decide whether the idle gap threshold is met.
    pub async fn seconds_since_last_assistant(&self, session_id: &str) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT CAST((julianday('now') - julianday(created_at)) * 86400 AS INTEGER) \
             FROM messages \
             WHERE session_id = ? AND role = 'assistant' AND compacted_at IS NULL \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(secs,)| secs))
    }
}

// ── Row types ───────────────────────────────────────────────────────────

/// Internal row type for sqlx deserialization.
#[derive(sqlx::FromRow)]
pub(crate) struct MessageRow {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: Option<String>,
    pub full_content: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub thinking_tokens: Option<i64>,
    pub created_at: Option<String>,
}

/// Session metadata for listing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct SessionInfoRow {
    pub id: String,
    pub agent_name: String,
    pub created_at: String,
    pub message_count: i64,
    pub total_tokens: i64,
    pub title: Option<String>,
    pub mode: Option<String>,
}

impl From<SessionInfoRow> for SessionInfo {
    fn from(r: SessionInfoRow) -> Self {
        Self {
            id: r.id,
            agent_name: r.agent_name,
            created_at: r.created_at,
            message_count: r.message_count,
            total_tokens: r.total_tokens,
            title: r.title,
            mode: r.mode,
        }
    }
}

impl From<MessageRow> for Message {
    fn from(r: MessageRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            role: r.role.parse().unwrap_or(Role::User),
            content: r.content,
            full_content: r.full_content,
            tool_calls: r.tool_calls,
            tool_call_id: r.tool_call_id,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
            thinking_tokens: r.thinking_tokens,
            created_at: r.created_at,
        }
    }
}
