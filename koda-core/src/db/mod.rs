//! SQLite persistence layer.
//!
//! Implements the [`crate::persistence::Persistence`] trait for SQLite via sqlx.
//! Uses WAL mode for concurrent read/write access.
//!
//! ## Database location
//!
//! - **Default**: `~/.config/koda/koda.db`
//! - Schema is auto-migrated on startup
//! - WAL mode enables concurrent reads (main session + sub-agents)
//!
//! ## What's stored
//!
//! - **Conversation history** — all messages, tool calls, and results
//! - **Sessions** — session metadata, timestamps, model info
//! - **File ownership** — which files Koda created (for auto-approve Delete)
//! - **Progress entries** — survive compaction for persistent tracking
//! - **KV store** — settings (last provider) and API keys (#693)
//! - **Input history** — REPL command history (#693)
//!
//! ## Module layout
//!
//! - **mod.rs** — `Database` struct, init/open, schema migrations, row types
//! - **queries.rs** — `Persistence` trait implementation (all SQL queries)

mod context_cache;
pub mod queries;
#[cfg(test)]
mod tests;

pub(crate) use context_cache::{ContextCache, ContextCacheEntry};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// Re-export persistence types for backward compatibility.
pub use crate::persistence::{
    CompactedStats, Message, Persistence, Role, SessionEvent, SessionInfo, SessionUsage,
};

/// Wrapper around the SQLite connection pool.
///
/// `Clone` is cheap — `SqlitePool` is internally `Arc`-shared, so a
/// clone bumps a refcount, not a real connection. This matters for
/// [`crate::engine::sink::PersistingSink`], which needs an owned
/// handle to spawn fire-and-forget DB writes from sink emissions.
///
/// `context_cache` and `compaction_gen` are shared via `Arc` so cloned
/// handles observe the same per-session cache state
/// (#1166 audit item A).
#[derive(Debug, Clone)]
pub struct Database {
    pub(crate) pool: SqlitePool,
    /// Per-session sanitized-context cache. See `context_cache` module.
    pub(crate) context_cache: std::sync::Arc<ContextCache>,
    /// Monotonic counter bumped on every `compact_session` call.
    /// Snapshot mismatch invalidates a `ContextCacheEntry`.
    pub(crate) compaction_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Get the koda config directory (~/.config/koda/).
///
/// **#1109 F1**: reads via [`crate::runtime_env::get`] (thread-safe with
/// `mask`/fallback semantics) instead of `std::env::var`. Tests can now
/// inject or hide these keys without `unsafe { set_var }`.
pub fn config_dir() -> Result<std::path::PathBuf> {
    let base = crate::runtime_env::get("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            // Unix: $HOME/.config  (XDG Base Directory spec fallback)
            crate::runtime_env::get("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })
        .or({
            // Windows: %APPDATA%  (e.g. C:\Users\Alice\AppData\Roaming)
            #[cfg(windows)]
            {
                crate::runtime_env::get("APPDATA").map(std::path::PathBuf::from)
            }
            #[cfg(not(windows))]
            {
                None
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot determine config directory \
                 (set XDG_CONFIG_HOME, HOME, or APPDATA)"
            )
        })?;
    Ok(base.join("koda"))
}

impl Database {
    /// Access the underlying connection pool (for tests and raw queries).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Drop the cached `load_context` snapshot for `session_id`, if any.
    ///
    /// (#1166) Production code never needs to call this — the cache
    /// invalidates itself on compaction and session deletion. Tests
    /// that mutate the `messages` table out-of-band (e.g. raw SQL,
    /// retroactive completion) can call this to ensure the next
    /// `load_context` does a full reload.
    pub fn clear_context_cache_for(&self, session_id: &str) {
        self.context_cache.invalidate(session_id);
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

        let db = Self::open(&db_path).await?;

        // Ensure restrictive permissions — DB contains API keys and
        // conversation history that may include secrets (#693).
        #[cfg(unix)]
        Self::set_db_permissions(&db_path);

        Ok(db)
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
        Ok(Self {
            pool,
            context_cache: std::sync::Arc::new(ContextCache::new()),
            compaction_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
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
                thinking_content TEXT,
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

        // Global key-value store (#693): replaces settings.toml and keys.toml.
        // Keys are namespaced by convention:
        //   - `setting:*`  — last-used provider, etc.
        //   - `apikey:*`   — API keys (GEMINI_API_KEY, etc.)
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .execute(pool)
        .await?;

        // REPL input history (#693): replaces ~/.config/koda/history.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS input_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input TEXT NOT NULL,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .execute(pool)
        .await?;

        // Session events (#1108 P1b/P2a): non-message engine events that
        // matter for debugging — `EngineEvent::Info`, `ChildTaskUpdate`, and
        // sub-agent inner-trace lines. Pre-#1108 these were sink-only and
        // never reached the transcript export, leaving the reader unable
        // to tell what a bg sub-agent was doing during its wait window.
        //
        // `parent_tool_call_id` is set for sub-agent-emitted events so the
        // renderer can fold them under the parent's `InvokeAgent` tool
        // result. NULL for top-level events.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS session_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                parent_tool_call_id TEXT,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_session_events_session_id \
             ON session_events(session_id, id);",
        )
        .execute(pool)
        .await?;

        Ok(())
    }

    /// Set koda.db file permissions to 0600 (owner-only).
    ///
    /// The DB contains API keys and conversation history that may include
    /// secrets. Restrictive permissions prevent other local users from reading.
    #[cfg(unix)]
    fn set_db_permissions(db_path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(db_path, perms) {
            tracing::warn!("Failed to set 0600 on {}: {e}", db_path.display());
        }
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
                    cache_read_tokens, cache_creation_tokens, thinking_tokens, thinking_content,
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
    pub thinking_content: Option<String>,
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
            thinking_content: r.thinking_content,
            created_at: r.created_at,
        }
    }
}

/// Internal row type for the `session_events` table (#1108 P1b/P2a).
#[derive(sqlx::FromRow)]
pub(crate) struct SessionEventRow {
    pub id: i64,
    pub session_id: String,
    pub kind: String,
    pub payload: String,
    pub parent_tool_call_id: Option<String>,
    pub created_at: Option<String>,
}

impl From<SessionEventRow> for SessionEvent {
    fn from(r: SessionEventRow) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            kind: r.kind,
            payload: r.payload,
            parent_tool_call_id: r.parent_tool_call_id,
            created_at: r.created_at,
        }
    }
}

// ── Global KV store (#693) ─────────────────────────────────────────────────────────

impl Database {
    /// Get a value from the global KV store.
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM kv_store WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(v,)| v))
    }

    /// Set a value in the global KV store (upsert).
    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO kv_store (key, value, updated_at) VALUES (?, ?, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a key from the global KV store.
    pub async fn kv_delete(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM kv_store WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get all KV entries matching a prefix (e.g. `"apikey:"`).
    pub async fn kv_list_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let pattern = format!("{prefix}%");
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM kv_store WHERE key LIKE ?")
                .bind(&pattern)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}

// ── Input history (#693) ─────────────────────────────────────────────────────────

/// Maximum number of input history entries to keep.
const MAX_INPUT_HISTORY: i64 = 500;

impl Database {
    /// Append an input to the history.
    pub async fn history_push(&self, input: &str) -> Result<()> {
        sqlx::query("INSERT INTO input_history (input) VALUES (?)")
            .bind(input)
            .execute(&self.pool)
            .await?;

        // Trim old entries beyond the cap.
        sqlx::query(
            "DELETE FROM input_history WHERE id NOT IN (
                SELECT id FROM input_history ORDER BY id DESC LIMIT ?
            )",
        )
        .bind(MAX_INPUT_HISTORY)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Load all history entries, oldest first.
    pub async fn history_load(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT input FROM input_history ORDER BY id ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(s,)| s).collect())
    }
}
