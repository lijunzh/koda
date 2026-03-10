//! SQLite persistence layer.
//!
//! Implements `Persistence` trait for SQLite via sqlx.
//! Uses WAL mode for concurrent access.

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

// Re-export types from persistence for backward compatibility.
pub use crate::persistence::{Message, Persistence, Role, SessionInfo, SessionUsage};

/// Wrapper around the SQLite connection pool.
#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
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
            .create_if_missing(true);

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

        sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_role_id ON messages(role, id DESC);")
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

        // Text column migrations
        for (col, col_type) in &[("agent_name", "TEXT")] {
            let sql = format!("ALTER TABLE messages ADD COLUMN {col} {col_type}");
            if let Err(e) = sqlx::query(&sql).execute(pool).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(e.into());
                }
            }
        }

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

        // Additive migration: add project_root to sessions
        let sql = "ALTER TABLE sessions ADD COLUMN project_root TEXT";
        if let Err(e) = sqlx::query(sql).execute(pool).await {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(e.into());
            }
        }

        // Phase transition flow log (#320 Phase 2).
        // Survives compaction — separate from conversation history.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS phase_transitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                iteration INTEGER NOT NULL,
                from_phase TEXT NOT NULL,
                to_phase TEXT NOT NULL,
                trigger TEXT,
                autonomy TEXT,
                review_depth TEXT,
                human_response TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                FOREIGN KEY(session_id) REFERENCES sessions(id)
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_phase_transitions_session \
             ON phase_transitions(session_id);",
        )
        .execute(pool)
        .await?;

        // Review records — child table of phase_transitions.
        // Only SelfReview and PeerReview create rows (not FastPath).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS review_records (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                phase_transition_id INTEGER NOT NULL REFERENCES phase_transitions(id),
                review_depth TEXT NOT NULL CHECK(review_depth IN ('self_review', 'peer_review')),
                reviewer_model TEXT NOT NULL,
                planner_model TEXT NOT NULL,
                plan_summary TEXT NOT NULL,
                reviewer_verdict TEXT NOT NULL CHECK(reviewer_verdict IN ('approved', 'rejected', 'revised')),
                reviewer_reasoning TEXT,
                human_decision TEXT CHECK(human_decision IN ('accepted_plan', 'accepted_review', 'manual_edit', 'aborted')),
                gate_reason TEXT NOT NULL CHECK(gate_reason IN (
                    'destructive_floor', 'remote_action_floor', 'complexity_threshold',
                    'observer_auto', 'peer_review_disagreement', 're_plan_exhausted'
                )),
                created_at TEXT DEFAULT (datetime('now'))
            );",
        )
        .execute(pool)
        .await?;

        Ok(())
    }
}

// ── Private helpers ─────────────────────────────────────────────

/// Strip tool_calls from any assistant message whose tool calls have no
/// corresponding tool result messages following it.
fn fix_orphaned_tool_calls(messages: &mut [Message]) {
    let len = messages.len();
    if len == 0 {
        return;
    }

    // Walk backwards: find the last assistant message with tool_calls
    // and check if tool result messages follow it.
    let mut i = len;
    while i > 0 {
        i -= 1;
        if messages[i].role == Role::Assistant && messages[i].tool_calls.is_some() {
            // Check if the next message is a tool result
            let has_result = i + 1 < len && messages[i + 1].role == Role::Tool;
            if !has_result {
                messages[i].tool_calls = None;
            }
            break; // only need to fix the trailing orphan
        }
        // If we hit a non-tool, non-assistant message going backwards, stop
        if messages[i].role != Role::Tool {
            break;
        }
    }
}

/// Rough token estimate: ~4 chars per token (good enough for sliding window).
fn estimate_tokens(msg: &Message) -> usize {
    let content_len = msg.content.as_deref().map_or(0, |c| c.len());
    let tool_len = msg.tool_calls.as_deref().map_or(0, |c| c.len());
    ((content_len + tool_len) as f64 / crate::inference_helpers::CHARS_PER_TOKEN) as usize
        + crate::inference_helpers::PER_MESSAGE_OVERHEAD
}

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

        Ok(result.last_insert_rowid())
    }

    /// Load recent messages for a session, applying a sliding window.
    /// Returns messages newest-first, capped at `max_tokens` estimated usage.
    async fn load_context(&self, session_id: &str, max_tokens: usize) -> Result<Vec<Message>> {
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
        // Messages are prioritized: user/assistant messages kept before
        // old tool results, which get aggressively truncated.
        let mut budget = max_tokens;
        let mut window = Vec::new();
        let recency_threshold = 4; // keep full content for this many recent messages

        for (idx, mut msg) in rows.into_iter().enumerate() {
            // Priority-based truncation:
            // - Recent messages (< threshold): full content always
            // - Old tool results: aggressive truncation (200 chars)
            // - Old assistant text: moderate truncation (1000 chars)
            // - User messages: keep full (they're the source of intent)
            if idx >= recency_threshold {
                if msg.role == Role::Phase {
                    // Phase messages: keep only the human-readable summary when old.
                    // Strip the JSON metadata to save tokens.
                    if let Some(ref content) = msg.content
                        && let Some(nl) = content.find('\n')
                    {
                        msg.content = Some(content[..nl].to_string());
                    }
                } else if msg.role == Role::Tool
                    && let Some(ref content) = msg.content
                    && content.len() > 200
                {
                    let mut end = 200.min(content.len());
                    while end > 0 && !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    msg.content = Some(format!(
                        "{}\n[truncated — {} chars. Re-read if needed.]",
                        &content[..end],
                        content.len()
                    ));
                } else if msg.role == Role::Assistant
                    && let Some(ref content) = msg.content
                    && content.len() > 1000
                {
                    let mut end = 1000.min(content.len());
                    while end > 0 && !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    msg.content = Some(format!(
                        "{}\n[truncated — {} chars]",
                        &content[..end],
                        content.len()
                    ));
                }
                // User messages: never truncated (they carry intent)
            }

            let estimated = estimate_tokens(&msg);
            if estimated > budget {
                break;
            }
            budget -= estimated;
            window.push(msg);
        }

        // Reverse so messages are in chronological order
        window.reverse();

        // Fix orphaned tool calls from interrupted sessions: if the last message
        // is an assistant message with tool_calls but no subsequent tool results,
        // strip the tool_calls so the LLM doesn't see inconsistent state.
        // This happens when a session was interrupted between saving the assistant
        // response and executing/saving tool results.
        fix_orphaned_tool_calls(&mut window);

        Ok(window)
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

        // Get all message IDs ordered oldest→newest
        let all_ids: Vec<(i64,)> =
            sqlx::query_as("SELECT id FROM messages WHERE session_id = ? ORDER BY id ASC")
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
            let sql =
                format!("DELETE FROM messages WHERE session_id = ? AND id IN ({placeholders})");
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

        // Reclaim freed pages from the bulk deletion.
        sqlx::query("PRAGMA incremental_vacuum")
            .execute(&self.pool)
            .await?;

        Ok(deleted_count)
    }

    /// Check if the last message in a session is a tool call awaiting a response.
    /// Used to defer compaction during active tool execution.
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool> {
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

    // ── Phase transition flow log ────────────────────────────

    /// Record a phase transition in the flow log.
    async fn insert_phase_transition(
        &self,
        session_id: &str,
        iteration: u32,
        from_phase: &str,
        to_phase: &str,
        trigger: Option<&str>,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO phase_transitions \
             (session_id, iteration, from_phase, to_phase, trigger) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(session_id)
        .bind(iteration as i64)
        .bind(from_phase)
        .bind(to_phase)
        .bind(trigger)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// Insert a review record (child of a phase transition).
    #[allow(clippy::too_many_arguments)]
    async fn insert_review_record(
        &self,
        phase_transition_id: i64,
        review_depth: &str,
        reviewer_model: &str,
        planner_model: &str,
        plan_summary: &str,
        reviewer_verdict: &str,
        reviewer_reasoning: Option<&str>,
        human_decision: Option<&str>,
        gate_reason: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO review_records \
             (phase_transition_id, review_depth, reviewer_model, planner_model, \
              plan_summary, reviewer_verdict, reviewer_reasoning, human_decision, gate_reason) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(phase_transition_id)
        .bind(review_depth)
        .bind(reviewer_model)
        .bind(planner_model)
        .bind(plan_summary)
        .bind(reviewer_verdict)
        .bind(reviewer_reasoning)
        .bind(human_decision)
        .bind(gate_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load a compact phase flow summary for a session.
    ///
    /// Returns a string like: `Observe(3) → Plan(1) → Review(1) → Act(7)`
    async fn phase_flow_summary(&self, session_id: &str) -> Result<String> {
        let rows: Vec<PhaseTransitionRow> = sqlx::query_as(
            "SELECT to_phase, COUNT(*) as count \
             FROM phase_transitions \
             WHERE session_id = ? \
             GROUP BY to_phase \
             ORDER BY MIN(id)",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;

        if rows.is_empty() {
            return Ok(String::new());
        }

        let parts: Vec<String> = rows
            .iter()
            .map(|r| format!("{}({})", r.to_phase, r.count))
            .collect();
        Ok(parts.join(" \u{2192} "))
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

/// Internal row type for phase transition queries.
#[derive(sqlx::FromRow)]
struct PhaseTransitionRow {
    to_phase: String,
    count: i64,
}

/// Session metadata for listing.
#[derive(Debug, Clone, sqlx::FromRow)]
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
            role: r.role.parse().unwrap_or(Role::User),
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
        let db_path = tmp.path().join("test.db");
        let db = Database::open(&db_path).await.unwrap();
        (db, tmp)
    }

    #[tokio::test]
    async fn test_create_session() {
        let (db, _tmp) = setup().await;
        let id = db.create_session("default", _tmp.path()).await.unwrap();
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn test_insert_and_load_messages() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_sliding_window_truncates_old_messages() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
        let s1 = db.create_session("agent-a", _tmp.path()).await.unwrap();
        let s2 = db.create_session("agent-b", _tmp.path()).await.unwrap();

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
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
        db.create_session("agent-a", _tmp.path()).await.unwrap();
        db.create_session("agent-b", _tmp.path()).await.unwrap();
        db.create_session("agent-c", _tmp.path()).await.unwrap();

        let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
        assert_eq!(sessions.len(), 3);
        // Most recent first
        assert_eq!(sessions[0].agent_name, "agent-c");
    }

    #[tokio::test]
    async fn test_delete_session() {
        let (db, _tmp) = setup().await;
        let s1 = db.create_session("default", _tmp.path()).await.unwrap();
        db.insert_message(&s1, &Role::User, Some("hello"), None, None, None)
            .await
            .unwrap();

        assert!(db.delete_session(&s1).await.unwrap());

        let sessions = db.list_sessions(10, _tmp.path()).await.unwrap();
        assert!(sessions.is_empty());

        // Deleting again returns false
        assert!(!db.delete_session(&s1).await.unwrap());
    }

    #[tokio::test]
    async fn test_compact_session() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
        let system_msgs: Vec<_> = msgs.iter().filter(|m| m.role == Role::System).collect();
        assert_eq!(system_msgs.len(), 1);
        assert!(
            system_msgs[0]
                .content
                .as_ref()
                .unwrap()
                .contains("Summary of conversation")
        );

        // Check that there's a continuation hint as assistant
        let assistant_msgs: Vec<_> = msgs.iter().filter(|m| m.role == Role::Assistant).collect();
        assert!(
            assistant_msgs
                .iter()
                .any(|m| m.content.as_deref().unwrap_or("").contains("compacted")),
            "Expected a continuation hint from assistant"
        );

        // The 2 preserved messages should still be there
        let preserved: Vec<_> = msgs
            .iter()
            .filter(|m| m.content.as_deref().is_some_and(|c| c.starts_with("msg ")))
            .collect();
        assert_eq!(preserved.len(), 2);
    }

    #[tokio::test]
    async fn test_compact_preserves_zero() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

        for i in 0..6 {
            let role = if i % 2 == 0 {
                &Role::User
            } else {
                &Role::Assistant
            };
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
        assert_eq!(msgs.iter().filter(|m| m.role == Role::System).count(), 1);
        assert_eq!(msgs.iter().filter(|m| m.role == Role::Assistant).count(), 1);
    }

    #[tokio::test]
    async fn test_has_pending_tool_calls() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
    async fn test_fix_orphaned_tool_calls() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

        // Normal turn: user → assistant with tool_calls → tool result
        db.insert_message(&session, &Role::User, Some("hello"), None, None, None)
            .await
            .unwrap();
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("Let me read that."),
            Some(r#"[{"id":"tc1","name":"Read","arguments":"{}"}]"#),
            None,
            None,
        )
        .await
        .unwrap();
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

        // Interrupted turn: assistant with tool_calls but NO tool result
        db.insert_message(
            &session,
            &Role::Assistant,
            Some("I'll edit the file."),
            Some(r#"[{"id":"tc2","name":"Edit","arguments":"{}"}]"#),
            None,
            None,
        )
        .await
        .unwrap();

        let msgs = db.load_context(&session, 100_000).await.unwrap();

        // The first assistant's tool_calls should be preserved (has tool result)
        let first_asst = msgs
            .iter()
            .find(|m| m.content.as_deref() == Some("Let me read that."))
            .unwrap();
        assert!(
            first_asst.tool_calls.is_some(),
            "completed tool_calls should be preserved"
        );

        // The orphaned assistant's tool_calls should be stripped
        let orphaned = msgs
            .iter()
            .find(|m| m.content.as_deref() == Some("I'll edit the file."))
            .unwrap();
        assert!(
            orphaned.tool_calls.is_none(),
            "orphaned tool_calls should be stripped"
        );
    }

    #[test]
    fn test_fix_orphaned_tool_calls_unit() {
        fn msg(
            role: &str,
            content: Option<&str>,
            tool_calls: Option<&str>,
            tool_call_id: Option<&str>,
        ) -> Message {
            Message {
                id: 0,
                session_id: String::new(),
                role: role.parse().unwrap_or(Role::User),
                content: content.map(Into::into),
                tool_calls: tool_calls.map(Into::into),
                tool_call_id: tool_call_id.map(Into::into),
                prompt_tokens: None,
                completion_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                thinking_tokens: None,
            }
        }

        // No messages — no crash
        let mut empty: Vec<Message> = vec![];
        fix_orphaned_tool_calls(&mut empty);
        assert!(empty.is_empty());

        // Last message is user — no change
        let mut msgs = vec![msg("user", Some("hi"), None, None)];
        fix_orphaned_tool_calls(&mut msgs);
        assert!(msgs[0].tool_calls.is_none());

        // Last message is assistant with tool_calls, no tool result — stripped
        let mut msgs = vec![
            msg("user", Some("hi"), None, None),
            msg(
                "assistant",
                Some("doing it"),
                Some(r#"[{"id":"t1"}]"#),
                None,
            ),
        ];
        fix_orphaned_tool_calls(&mut msgs);
        assert!(msgs[1].tool_calls.is_none());

        // Last message is tool result — assistant tool_calls preserved
        let mut msgs = vec![
            msg("user", Some("hi"), None, None),
            msg("assistant", None, Some(r#"[{"id":"t1"}]"#), None),
            msg("tool", Some("ok"), None, Some("t1")),
        ];
        fix_orphaned_tool_calls(&mut msgs);
        assert!(msgs[1].tool_calls.is_some());
    }

    #[tokio::test]
    async fn test_session_metadata_and_todo() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

        // No metadata initially
        assert!(db.get_todo(&session).await.unwrap().is_none());
        assert!(
            db.get_metadata(&session, "anything")
                .await
                .unwrap()
                .is_none()
        );

        // Set and get todo
        db.set_todo(&session, "- [ ] Task 1\n- [x] Task 2")
            .await
            .unwrap();
        let todo = db.get_todo(&session).await.unwrap().unwrap();
        assert!(todo.contains("Task 1"));
        assert!(todo.contains("Task 2"));

        // Update (upsert) replaces the value
        db.set_todo(&session, "- [x] Task 1\n- [x] Task 2")
            .await
            .unwrap();
        let todo = db.get_todo(&session).await.unwrap().unwrap();
        assert!(todo.starts_with("- [x] Task 1"));

        // Generic metadata works too
        db.set_metadata(&session, "custom_key", "custom_value")
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata(&session, "custom_key")
                .await
                .unwrap()
                .unwrap(),
            "custom_value"
        );
    }

    #[tokio::test]
    async fn test_token_usage_empty_session() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

        let u = db.session_token_usage(&session).await.unwrap();
        assert_eq!(u.prompt_tokens, 0);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.api_calls, 0);
    }

    #[tokio::test]
    async fn test_last_assistant_message() {
        let (db, _tmp) = setup().await;
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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
        let session = db.create_session("default", _tmp.path()).await.unwrap();

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

    #[tokio::test]
    async fn test_phase_transitions_insert_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.db").as_path())
            .await
            .unwrap();
        let session = db.create_session("test", dir.path()).await.unwrap();

        db.insert_phase_transition(&session, 1, "Understanding", "Planning", Some("text_only"))
            .await
            .unwrap();
        db.insert_phase_transition(&session, 2, "Planning", "Reviewing", Some("text_only"))
            .await
            .unwrap();
        db.insert_phase_transition(&session, 3, "Reviewing", "Executing", Some("tool:Edit"))
            .await
            .unwrap();
        db.insert_phase_transition(&session, 5, "Executing", "Executing", Some("tool:Bash"))
            .await
            .unwrap();

        let summary = db.phase_flow_summary(&session).await.unwrap();
        assert!(summary.contains("Planning"));
        assert!(summary.contains("Reviewing"));
        assert!(summary.contains("Executing"));
        assert!(summary.contains("\u{2192}")); // arrow
    }

    #[tokio::test]
    async fn test_phase_flow_summary_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("test.db").as_path())
            .await
            .unwrap();
        let session = db.create_session("test", dir.path()).await.unwrap();

        let summary = db.phase_flow_summary(&session).await.unwrap();
        assert!(summary.is_empty());
    }
}
