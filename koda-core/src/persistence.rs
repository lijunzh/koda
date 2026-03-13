//! Persistence trait — the storage contract for koda.
//!
//! Types and trait definition for the storage layer. The engine
//! depends on this trait, not the concrete SQLite implementation.
//!
//! The default implementation is `Database` in `db.rs`.

use anyhow::Result;
use std::path::Path;

/// Message roles in the conversation.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum Role {
    /// System prompt.
    System,
    /// User message.
    User,
    /// Assistant (LLM) response.
    Assistant,
    /// Tool result.
    Tool,
}

impl Role {
    /// String representation for database storage.
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

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

/// A stored message row.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Message {
    /// Database row ID.
    pub id: i64,
    /// Session this message belongs to.
    pub session_id: String,
    /// Message role (system, user, assistant, tool).
    pub role: Role,
    /// Text content.
    pub content: Option<String>,
    /// Serialized tool calls JSON.
    pub tool_calls: Option<String>,
    /// ID of the tool call this responds to.
    pub tool_call_id: Option<String>,
    /// Input tokens for this message.
    pub prompt_tokens: Option<i64>,
    /// Output tokens for this message.
    pub completion_tokens: Option<i64>,
    /// Cached input tokens.
    pub cache_read_tokens: Option<i64>,
    /// Tokens written to cache.
    pub cache_creation_tokens: Option<i64>,
    /// Reasoning/thinking tokens.
    pub thinking_tokens: Option<i64>,
}

/// Token usage totals for a session.
#[derive(Debug, Clone, Default)]
pub struct SessionUsage {
    /// Total input tokens.
    pub prompt_tokens: i64,
    /// Total output tokens.
    pub completion_tokens: i64,
    /// Total cached input tokens.
    pub cache_read_tokens: i64,
    /// Total tokens written to cache.
    pub cache_creation_tokens: i64,
    /// Total reasoning/thinking tokens.
    pub thinking_tokens: i64,
    /// Number of API calls made.
    pub api_calls: i64,
}

/// Summary info for a stored session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session identifier.
    pub id: String,
    /// Agent name for this session.
    pub agent_name: String,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Total messages in the session.
    pub message_count: i64,
    /// Cumulative token count.
    pub total_tokens: i64,
}

/// Stats about compacted (archived) messages in the database.
#[derive(Debug, Clone, Default)]
pub struct CompactedStats {
    /// Number of compacted messages.
    pub message_count: i64,
    /// Number of sessions with compacted messages.
    pub session_count: i64,
    /// Approximate size in bytes of compacted message content.
    pub size_bytes: i64,
    /// ISO 8601 timestamp of the oldest compacted message.
    pub oldest: Option<String>,
}

/// Core storage contract for sessions, messages, and metadata.
#[async_trait::async_trait]
pub trait Persistence: Send + Sync {
    // ── Sessions ──

    /// Create a new session, returning its unique ID.
    async fn create_session(&self, agent_name: &str, project_root: &Path) -> Result<String>;
    /// List recent sessions for the given project root.
    async fn list_sessions(&self, limit: i64, project_root: &Path) -> Result<Vec<SessionInfo>>;
    /// Delete a session by ID. Returns `true` if it existed.
    async fn delete_session(&self, session_id: &str) -> Result<bool>;

    // ── Messages ──

    /// Insert a message into a session.
    async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::providers::TokenUsage>,
    ) -> Result<i64>;

    /// Insert a message with an explicit agent name (for sub-agent tracking).
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
    ) -> Result<i64>;

    /// Load active (non-compacted) conversation context for a session.
    async fn load_context(&self, session_id: &str) -> Result<Vec<Message>>;
    /// Load all messages in a session (no token limit).
    async fn load_all_messages(&self, session_id: &str) -> Result<Vec<Message>>;
    /// Recent user messages across all sessions (for startup hints).
    async fn recent_user_messages(&self, limit: i64) -> Result<Vec<String>>;
    /// Last assistant message in a session.
    async fn last_assistant_message(&self, session_id: &str) -> Result<String>;
    /// Last user message in a session.
    async fn last_user_message(&self, session_id: &str) -> Result<String>;
    /// Check if the session has unresolved tool calls.
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool>;

    // ── Token usage ──

    /// Token usage totals for a session.
    async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage>;
    /// Token usage broken down by agent name.
    async fn session_usage_by_agent(&self, session_id: &str)
    -> Result<Vec<(String, SessionUsage)>>;

    // ── Compaction ──

    /// Compact old messages into a summary, preserving the last N messages.
    async fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        preserve_count: usize,
    ) -> Result<usize>;

    // ── Purge ──

    /// Stats about compacted (archived) messages across all sessions.
    async fn compacted_stats(&self) -> Result<CompactedStats>;
    /// Permanently delete compacted messages older than `min_age_days`.
    /// Returns the number of messages deleted.
    async fn purge_compacted(&self, min_age_days: u32) -> Result<usize>;

    // ── Metadata ──

    /// Get a session metadata value by key.
    async fn get_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>>;
    /// Set a session metadata value.
    async fn set_metadata(&self, session_id: &str, key: &str, value: &str) -> Result<()>;
    /// Get the TODO list for a session.
    async fn get_todo(&self, session_id: &str) -> Result<Option<String>>;
    /// Set the TODO list for a session.
    async fn set_todo(&self, session_id: &str, content: &str) -> Result<()>;
}
