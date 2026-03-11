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
    System,
    User,
    Assistant,
    Tool,
    /// Phase transition log entry.
    Phase,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Phase => "phase",
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
            "phase" => Ok(Self::Phase),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

/// A stored message row.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: Role,
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

/// Summary info for a stored session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub agent_name: String,
    pub created_at: String,
    pub message_count: i64,
    pub total_tokens: i64,
}

/// Core storage contract for sessions, messages, and metadata.
#[async_trait::async_trait]
pub trait Persistence: Send + Sync {
    // ── Sessions ──

    async fn create_session(&self, agent_name: &str, project_root: &Path) -> Result<String>;
    async fn list_sessions(&self, limit: i64, project_root: &Path) -> Result<Vec<SessionInfo>>;
    async fn delete_session(&self, session_id: &str) -> Result<bool>;

    // ── Messages ──

    async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&crate::providers::TokenUsage>,
    ) -> Result<i64>;

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

    async fn load_context(&self, session_id: &str, max_tokens: usize) -> Result<Vec<Message>>;
    async fn load_all_messages(&self, session_id: &str) -> Result<Vec<Message>>;
    async fn recent_user_messages(&self, limit: i64) -> Result<Vec<String>>;
    async fn last_assistant_message(&self, session_id: &str) -> Result<String>;
    async fn last_user_message(&self, session_id: &str) -> Result<String>;
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool>;

    // ── Token usage ──

    async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage>;
    async fn session_usage_by_agent(&self, session_id: &str)
    -> Result<Vec<(String, SessionUsage)>>;

    // ── Compaction ──

    async fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        preserve_count: usize,
    ) -> Result<usize>;

    // ── Metadata ──

    async fn get_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>>;
    async fn set_metadata(&self, session_id: &str, key: &str, value: &str) -> Result<()>;
    async fn get_todo(&self, session_id: &str) -> Result<Option<String>>;
    async fn set_todo(&self, session_id: &str, content: &str) -> Result<()>;
}
