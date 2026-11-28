//! `SessionStore` — how sessions are stored: their messages, turns,
//! events and metadata. Codex calls the same boundary `ThreadStore`;
//! Koda says session where Codex says thread.
//!
//! Types and trait definition for the storage layer. The engine
//! depends on this trait, not the concrete SQLite implementation.
//!
//! The default implementation is [`crate::db::Database`], over SQLite.
//!
//! ## Design (DESIGN.md)
//!
//! - **Database Backend: SQLite + Store Traits (P3)**: This trait
//!   exists because P3 says the storage backend will change. But there's
//!   only one real implementation because P1 says we don't need a second
//!   one yet. When P1 and P3 conflict: P1 wins on timing, P3 wins on
//!   architecture.

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
    ///
    /// # Examples
    ///
    /// ```
    /// use koda_state::session_store::Role;
    ///
    /// assert_eq!(Role::User.as_str(), "user");
    /// assert_eq!(Role::Assistant.as_str(), "assistant");
    /// ```
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
    /// Text content (may be a summary for Bash results).
    pub content: Option<String>,
    /// Full untruncated output (only set for Bash tool results).
    pub full_content: Option<String>,
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
    /// Full thinking/reasoning text produced by Claude extended thinking.
    ///
    /// `None` for non-Claude models, or when thinking was disabled.
    /// Persisted so the content can be re-rendered on session resume.
    pub thinking_content: Option<String>,
    /// The model that wrote [`Self::thinking_content`]: reasoning text goes
    /// back only to that model. `None` on rows from before Koda recorded it.
    pub thinking_model: Option<String>,
    /// Signed thinking blocks, as JSON naming the model that wrote them; the
    /// provider decides which models get them back.
    pub thinking_blocks: Option<String>,
    /// Images sent with the message, as JSON (`[{media_type, base64}]`):
    /// replayed on every model call and after a resume. `None` for none.
    pub images: Option<String>,
    /// ISO 8601 creation timestamp.
    pub created_at: Option<String>,
}

/// Kind of [`SessionEvent`] payload.
///
/// Stored as a string in the `kind` column — the renderer dispatches
/// on it and parses `payload` accordingly. Keeping it open-ended (string)
/// rather than an enum at the DB layer means future event kinds don't
/// require a schema change.
pub mod session_event_kind {
    /// `EngineEvent::Info` — payload is the message text.
    pub const INFO: &str = "info";
    /// `EngineEvent::ChildTaskUpdate` — payload is JSON
    /// `{"task_id": u32, "spawner": Option<u32>, "status": AgentStatus}`.
    pub const BG_TASK_UPDATE: &str = "bg_task_update";
    /// One line of a sub-agent's `BufferingSink` trace — payload is
    /// the rendered line. `parent_tool_call_id` correlates back to the
    /// `InvokeAgent` tool call in the parent session.
    pub const SUB_AGENT_EVENT: &str = "sub_agent_event";
}

/// A non-message engine event persisted for transcript debuggability.
///
/// See [`session_event_kind`] for the supported kinds. Pre-#1108 these
/// were sink-only; the transcript export couldn't tell what a bg
/// sub-agent did during its wait window.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    /// Auto-incremented row id (also acts as a per-session sort key
    /// since events are appended in emission order).
    pub id: i64,
    /// Owning session.
    pub session_id: String,
    /// One of the constants in [`session_event_kind`].
    pub kind: String,
    /// Kind-specific payload (see [`session_event_kind`] doc).
    pub payload: String,
    /// For `SUB_AGENT_EVENT`: the `tool_call_id` of the `InvokeAgent`
    /// call that spawned the sub-agent. `None` for top-level events.
    pub parent_tool_call_id: Option<String>,
    /// ISO 8601 creation timestamp.
    pub created_at: Option<String>,
}

/// Metadata key on a sub-agent's session, holding the id of the session that
/// started it. The session's row holds it too ([`SessionInfo::parent_session_id`]);
/// the key stays for a Koda from before the column.
pub const PARENT_SESSION_KEY: &str = "parent_session";

/// How a session came to be, as Codex's `SessionSource` says of a thread.
/// Stored as text; a session from before it was recorded has none.
pub mod source {
    /// Started in the terminal UI.
    pub const CLI: &str = "cli";
    /// Started by `koda exec`.
    pub const EXEC: &str = "exec";
    /// Started by another client of the server, such as a desktop app.
    pub const APP_SERVER: &str = "appServer";
    /// Started by a session, for one of its sub-agents.
    pub const SUB_AGENT: &str = "subAgent";
}

/// Which sessions to list, in what order, and from where, as Codex's
/// `thread/list` filters, sorts and pages. Ephemeral sessions are never
/// listed, and archived ones only when asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionListQuery {
    /// Only sessions of these folders, and those from before folders were
    /// recorded; `None` lists every folder's.
    pub project_roots: Option<Vec<String>>,
    /// Only sessions of these [`source`]s; `None` lists every source's.
    pub sources: Option<Vec<String>>,
    /// With [`Self::sources`]: also sessions whose source was never
    /// recorded.
    pub unknown_source: bool,
    /// Only sessions of these providers; `None` lists every provider's.
    pub providers: Option<Vec<String>>,
    /// Only archived sessions, not only the ones that aren't, as Codex's
    /// `thread/list` takes `archived`.
    pub archived: bool,
    /// Only sessions whose title or preview contains this text, as Codex's
    /// `thread/list` takes `searchTerm`. ASCII letters match in either
    /// case; other letters only as written.
    pub search_term: Option<String>,
    /// Order by when each was last written, not when it was created.
    pub by_updated: bool,
    /// Oldest first, not newest first.
    pub ascending: bool,
    /// Go on after the session with this sort time (Unix seconds) and
    /// [`SessionInfo::sequence`]: the last of the page before.
    pub after: Option<(i64, i64)>,
    /// How many at most.
    pub limit: i64,
}

/// How many characters of its first prompt a session keeps as its preview.
pub const PREVIEW_CHARS: usize = 1000;

/// How a search over sessions' text compares one character: by its lower
/// case, so a term finds its text whichever case either is in.
///
/// One character always folds to one, and only an ASCII character folds to
/// an ASCII one, so a place in the folded text is the same place in the
/// text, and SQLite's `LIKE`, which ignores case for ASCII only, can stand
/// in for it there ([`SessionStore::search_candidates_page`]). That leaves
/// two characters as they are: U+0130 (İ), whose lower case is two
/// characters, and U+212A (the Kelvin sign), whose lower case is `k`.
pub fn search_fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(one), None) if !one.is_ascii() => one,
        _ => c,
    }
}

/// Where a new session came from, recorded on its row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOrigin {
    /// One of [`source`]'s, when known.
    pub source: Option<String>,
    /// The client that started it, by the name it gave in `initialize`, as
    /// Codex records a thread's originator.
    pub originator: Option<String>,
    /// The session whose sub-agent this is.
    pub parent_session_id: Option<String>,
    /// Not meant to be kept or listed: a side conversation or a review,
    /// which its client deletes, as Codex marks a thread ephemeral.
    pub ephemeral: bool,
}

/// One stored part of a turn: the turn as it started or ended, or an item
/// as it completed, which the client host writes as a client saw them, so
/// resuming a session replays its turns exactly, as Codex's rollout file
/// does; or what the turn ran with, a [`TurnContextItem`], which core
/// writes. The payload is JSON; core reads none of it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// The turn it belongs to.
    pub turn_id: String,
    /// What it is: `turn`, `item` or [`TURN_CONTEXT_KIND`].
    pub kind: String,
    /// The JSON.
    pub payload: String,
}

/// The [`TurnRecord::kind`] of a turn as it started or ended.
pub const TURN_KIND: &str = "turn";

/// The [`TurnRecord::kind`] of a completed item.
pub const ITEM_KIND: &str = "item";

/// The [`TurnRecord::kind`] of a [`TurnContextItem`].
pub const TURN_CONTEXT_KIND: &str = "context";

/// The [`TurnRecord::kind`] of a turn's [`MessageBoundary`], stored as the
/// turn starts so the session can be rewound to before it.
pub const REWIND_KIND: &str = "rewind";

/// Where a session's messages stood at a point: the last message's id, and
/// how many were compacted. A compaction after the point changes the count,
/// and a rewind across it is refused: the summary that replaced the
/// history is gone with the turns after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBoundary {
    /// The last message's id, 0 for none.
    pub last_message_id: i64,
    /// How many messages were compacted.
    pub compacted: i64,
}

/// What a turn ran with, stored once per turn as a [`TURN_CONTEXT_KIND`]
/// record, as Codex stores a `TurnContextItem` in its rollout once per
/// user turn: so a session can be debugged with what each turn actually
/// used, not only the session's latest settings. Like Codex's, it is
/// storage only; the protocol's turns don't carry it.
///
/// Written by core from the turn it is about to run
/// (`koda_core::turn_context::context_item`), so it holds what the turn
/// used: a settings change made during a turn applies to the next.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnContextItem {
    /// The turn it belongs to; a sub-agent's is its `InvokeAgent` call.
    pub turn_id: String,
    /// Koda's version. Codex keeps its version once per session; a Koda
    /// session can be resumed after an upgrade, so each turn says.
    pub koda_version: String,
    /// The directory the turn worked in: the project, or a sub-agent's
    /// worktree.
    pub cwd: String,
    /// The agent running the turn.
    pub agent_name: String,
    /// The provider's type, such as `anthropic`.
    pub provider: String,
    /// The provider's endpoint.
    pub base_url: String,
    /// The model the turn called.
    pub model: String,
    /// The output limit sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// The temperature sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// The thinking budget sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    /// The reasoning effort sent, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The context window the turn compacted against, in tokens.
    pub context_window: usize,
    /// The trust mode the turn ran in: `plan`, `safe` or `auto`.
    pub trust_mode: String,
    /// The tools offered to the model, built-in and MCP, by name.
    pub tools: Vec<String>,
    /// SHA-256 of the system prompt, so a change to it shows without
    /// storing it every turn. `None` for a compaction, whose prompt is
    /// Koda's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sha256: Option<String>,
    /// SHA-256 of the project memory sent with it; `None` for a compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_memory_sha256: Option<String>,
}

impl TurnContextItem {
    /// Store it for `session_id`. A failure is logged and the turn goes on:
    /// the record is for debugging, and a turn shouldn't fail over it.
    pub async fn store(&self, db: &dyn SessionStore, session_id: &str) {
        let payload = match serde_json::to_string(self) {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!("couldn't encode turn {}'s context: {e}", self.turn_id);
                return;
            }
        };
        let record = TurnRecord {
            turn_id: self.turn_id.clone(),
            kind: TURN_CONTEXT_KIND.to_string(),
            payload,
        };
        if let Err(e) = db.append_turn_records(session_id, &[record]).await {
            tracing::warn!("couldn't store turn {}'s context: {e:#}", self.turn_id);
        }
    }
}

/// The tokens one model call used, as a message row stores them. The
/// engine converts its provider's usage into this when it writes a reply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageUsage {
    /// Input tokens.
    pub prompt_tokens: i64,
    /// Output tokens.
    pub completion_tokens: i64,
    /// Input tokens read from the provider's cache.
    pub cache_read_tokens: i64,
    /// Input tokens written to the provider's cache.
    pub cache_creation_tokens: i64,
    /// Output tokens spent reasoning.
    pub thinking_tokens: i64,
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
    /// Auto-generated title from first user message.
    pub title: Option<String>,
    /// Last active approval mode (for restore on resume).
    pub mode: Option<String>,
    /// Creation time, in Unix seconds.
    pub created_unix: i64,
    /// When it was last written (its creation before that), in Unix seconds.
    pub updated_unix: i64,
    /// The start of the session's first user message, if it has one
    /// ([`PREVIEW_CHARS`] of it).
    pub preview: Option<String>,
    /// The project it was started in; `None` for sessions older than the column.
    pub project_root: Option<String>,
    /// The provider's id, when the session recorded it.
    pub provider: Option<String>,
    /// The provider's endpoint, when recorded.
    pub base_url: Option<String>,
    /// The model choice as made, an alias or an id, when recorded.
    pub model: Option<String>,
    /// How it came to be, one of [`source`]'s; `None` from before it was
    /// recorded.
    pub source: Option<String>,
    /// The client that started it, by its `initialize` name, when recorded.
    pub originator: Option<String>,
    /// The session it was forked from.
    pub forked_from_id: Option<String>,
    /// The session whose sub-agent it is.
    pub parent_session_id: Option<String>,
    /// A side conversation or a review, not meant to be listed.
    pub ephemeral: bool,
    /// When it was archived, in Unix seconds; `None` for one that isn't.
    pub archived_unix: Option<i64>,
    /// Its place in the order sessions were made, which orders the
    /// sessions of one second in a list.
    pub sequence: i64,
}

/// Core storage contract for sessions, messages, and metadata.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    // ── Sessions ──

    /// Create a new session, returning its unique ID.
    async fn create_session(&self, agent_name: &str, project_root: &Path) -> Result<String> {
        self.create_session_from(agent_name, project_root, &SessionOrigin::default())
            .await
    }
    /// [`Self::create_session`], recording where it came from.
    async fn create_session_from(
        &self,
        agent_name: &str,
        project_root: &Path,
        origin: &SessionOrigin,
    ) -> Result<String>;
    /// List recent sessions for the given project root; archived ones are
    /// left out.
    async fn list_sessions(&self, limit: i64, project_root: &Path) -> Result<Vec<SessionInfo>> {
        self.list_sessions_from(limit, 0, project_root).await
    }
    /// [`Self::list_sessions`] after skipping the `offset` most recent, for
    /// listing page by page.
    async fn list_sessions_from(
        &self,
        limit: i64,
        offset: i64,
        project_root: &Path,
    ) -> Result<Vec<SessionInfo>>;
    /// The sessions `query` asks for: one page, in its order. A page ends
    /// at a session, not a count, so sessions made meanwhile don't shift it.
    async fn list_sessions_page(&self, query: &SessionListQuery) -> Result<Vec<SessionInfo>>;
    /// [`Self::list_sessions_page`], narrowed to the sessions whose
    /// conversation may contain `term`: the candidates of a search, as
    /// Codex's finds the rollout files that mention a term before it reads
    /// any. The caller reads each one and decides.
    ///
    /// A page never leaves out a session in which `term` occurs, compared
    /// with [`search_fold`], inside one piece of text a turn keeps: a
    /// message, a tool's name, an argument's name or value, or a tool's
    /// output. It may hold sessions without it. Where the store can't tell
    /// cheaply and for certain (a letter outside ASCII that has another
    /// case, a line break or another control character), it narrows
    /// nothing, and the page is `query`'s own.
    async fn search_candidates_page(
        &self,
        query: &SessionListQuery,
        term: &str,
    ) -> Result<Vec<SessionInfo>>;
    /// One session by its full id, or `None` if there is none.
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionInfo>>;
    /// The sessions whose id starts with `prefix`, `limit` of them at most
    /// in no order, of every kind: what a shortened id may name.
    async fn sessions_with_id_prefix(&self, prefix: &str, limit: i64) -> Result<Vec<SessionInfo>>;

    /// Copy `source` into a new session named `title`, and return its id:
    /// the session's provider, model and mode, its messages (token counts
    /// included), turn records, metadata and events, as Codex's
    /// `thread/fork` copies a thread's rollout. The copy belongs to
    /// `project_root`, where it opens, as a Codex fork takes the cwd it
    /// starts in (`koda fork --worktree` opens it in a new checkout). One
    /// transaction; `source` is left as it was. The copy records `source`
    /// as what it was forked from, and `origin` as who forked it.
    async fn fork_session(
        &self,
        source: &str,
        title: &str,
        project_root: &Path,
        origin: &SessionOrigin,
    ) -> Result<String>;
    /// Delete a session by ID. Returns `true` if it existed.
    async fn delete_session(&self, session_id: &str) -> Result<bool>;
    /// Archive a session, and with it the sessions of the sub-agents it
    /// ran, and theirs, as Codex's `thread/archive` archives a thread's
    /// spawned descendants. Nothing stored is changed but when each was
    /// archived. Returns the sessions this archived: `session_id` first,
    /// then its descendants, the deepest first; one already archived isn't
    /// among them, and an empty list means `session_id` already was, or
    /// names no session.
    async fn archive_session(&self, session_id: &str) -> Result<Vec<String>>;
    /// Bring an archived session back, with its archived descendants.
    /// Returns the sessions this restored, `session_id` first; an empty
    /// list means it wasn't archived, or names no session.
    async fn unarchive_session(&self, session_id: &str) -> Result<Vec<String>>;
    /// Set the auto-generated title for a session.
    async fn set_session_title(&self, session_id: &str, title: &str) -> Result<()>;
    /// Persist the current approval mode for a session (restored on resume).
    async fn set_session_mode(&self, session_id: &str, mode: &str) -> Result<()>;
    /// Record the session's provider, endpoint and model choice (an alias
    /// or an id), restored on resume.
    async fn set_session_model(
        &self,
        session_id: &str,
        provider: &str,
        base_url: &str,
        model: &str,
    ) -> Result<()>;
    /// Get the stored approval mode for a session.
    async fn get_session_mode(&self, session_id: &str) -> Result<Option<String>>;

    // ── Messages ──

    /// Insert a message into a session.
    async fn insert_message(
        &self,
        session_id: &str,
        role: &Role,
        content: Option<&str>,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        usage: Option<&MessageUsage>,
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
        usage: Option<&MessageUsage>,
        agent_name: Option<&str>,
    ) -> Result<i64>;

    /// Insert a tool message with full (untruncated) output stored separately.
    ///
    /// `content` holds the model-facing summary; `full_content` holds the
    /// complete output for later retrieval via RecallContext.
    #[allow(clippy::too_many_arguments)]
    async fn insert_tool_message_with_full(
        &self,
        session_id: &str,
        content: &str,
        tool_call_id: &str,
        full_content: &str,
    ) -> Result<i64>;

    /// Load active (non-compacted) conversation context for a session.
    async fn load_context(&self, session_id: &str) -> Result<Vec<Message>>;
    /// Load all messages in a session (no token limit).
    async fn load_all_messages(&self, session_id: &str) -> Result<Vec<Message>>;
    /// The model's final text answer since the last user message, or empty.
    async fn last_assistant_message(&self, session_id: &str) -> Result<String>;
    /// Check if the session has unresolved tool calls.
    async fn has_pending_tool_calls(&self, session_id: &str) -> Result<bool>;

    /// Mark an assistant message as fully delivered.
    ///
    /// Sets `completed_at = CURRENT_TIMESTAMP`. Only called after a legitimate
    /// `StreamChunk::Done` — not after user cancellation or a network error.
    /// A `NULL` `completed_at` means the message is in-progress or was interrupted.
    async fn mark_message_complete(&self, message_id: i64) -> Result<()>;

    /// Atomically copy a slice of pre-loaded messages into a session.
    ///
    /// Used by `fork` sub-agent dispatch (#1022 B20). Pre-fix, the fork
    /// path looped over `SessionStore::insert_message` row-by-row — each
    /// iteration was 2 queries (INSERT + UPDATE last_accessed_at) plus,
    /// for assistant rows, a separate `mark_message_complete` (1 more
    /// query). For an N-message parent history that's ~3N round-trips
    /// to SQLite, each its own commit, on the synchronous fork hot path.
    ///
    /// This method:
    /// - Wraps every INSERT in a single transaction (one fsync at COMMIT
    ///   instead of N).
    /// - Sets `completed_at = datetime('now')` inline at INSERT time for
    ///   `Role::Assistant` rows, eliminating the separate
    ///   `mark_message_complete` round-trip per row.
    /// - Updates `last_accessed_at` on the destination session exactly
    ///   once at the end (not per-row).
    ///
    /// Token usage stats are intentionally **not** copied: fork creates a
    /// new conversation perspective and cost-attributing parent tokens
    /// to the child would double-count session usage. Same policy as the
    /// pre-fix loop, which passed `usage = None` for every copied row.
    async fn copy_messages_into_session(
        &self,
        dst_session: &str,
        messages: &[Message],
    ) -> Result<()>;

    /// Persist the reasoning text `model` wrote for an assistant message.
    /// Providers that return none leave `thinking_content` NULL.
    async fn update_message_thinking_content(
        &self,
        message_id: i64,
        model: &str,
        content: &str,
    ) -> Result<()>;

    /// Persist signed thinking blocks for an assistant message. Call it before
    /// `mark_message_complete`, so no cached context holds the row yet.
    async fn update_message_thinking_blocks(&self, message_id: i64, blocks: &str) -> Result<()>;

    /// Store `images` (JSON) on the latest user message of `session_id`:
    /// the prompt a turn was started with.
    async fn set_last_user_message_images(&self, session_id: &str, images: &str) -> Result<()>;

    /// Store `images` (JSON) on message `message_id` of `session_id`: the
    /// images a tool returned with its result.
    async fn set_message_images(
        &self,
        session_id: &str,
        message_id: i64,
        images: &str,
    ) -> Result<()>;

    // ── Token usage ──

    /// Token usage totals for a session.
    ///
    /// Sub-agents keep their messages in sessions of their own, linked by
    /// [`PARENT_SESSION_KEY`], and count toward the session that started them.
    async fn session_token_usage(&self, session_id: &str) -> Result<SessionUsage>;
    /// What the session's own latest model call used, if it has made one:
    /// how full its context was then. Its sub-agents' calls are theirs.
    async fn last_call_usage(&self, session_id: &str) -> Result<Option<SessionUsage>>;
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

    /// Compact every message older than `keep_from_id`, and any earlier
    /// summary, into a summary.
    ///
    /// Takes a row id because `preserve_count` counts raw rows, which include
    /// any that `load_context` prunes. `usage` is what the summary cost, stored
    /// on its row so session totals include it.
    async fn compact_session_before(
        &self,
        session_id: &str,
        summary: &str,
        keep_from_id: i64,
        usage: Option<&MessageUsage>,
    ) -> Result<usize>;

    // ── Metadata ──

    /// Get a session metadata value by key.
    async fn get_metadata(&self, session_id: &str, key: &str) -> Result<Option<String>>;
    /// Set a session metadata value.
    async fn set_metadata(&self, session_id: &str, key: &str, value: &str) -> Result<()>;
    /// The sessions whose [`PARENT_SESSION_KEY`] is `parent`: the
    /// sub-agents it ran, oldest first.
    async fn child_session_ids(&self, parent: &str) -> Result<Vec<String>>;
    /// Everything stored for a session, as the backend holds it, for a
    /// feedback report: its session row, metadata, every message (compacted
    /// ones too), turn records and events, each a list of rows with every
    /// column. Never API keys or input history, which aren't a session's.
    /// Koda's stand-in for the rollout file Codex attaches.
    async fn export_session(&self, session_id: &str) -> Result<serde_json::Value>;

    // ── Turn records (6h) ──

    /// Append `records` to the session's turn records, in order.
    async fn append_turn_records(&self, session_id: &str, records: &[TurnRecord]) -> Result<()>;
    /// The session's turn records, in the order they were written.
    async fn load_turn_records(&self, session_id: &str) -> Result<Vec<TurnRecord>>;
    /// Where the session's messages stand: the last message's id, and how
    /// many are compacted. A turn stores it as it starts
    /// ([`REWIND_KIND`]), so it can be rewound.
    async fn message_boundary(&self, session_id: &str) -> Result<MessageBoundary>;
    /// Take turn `turn_id` and everything after it out of the session: its
    /// messages after `boundary` and its turn records from the turn's first
    /// on, as Codex's `thread/revert` replaces a thread's history with the
    /// part before one turn. Files are untouched.
    async fn rewind(
        &self,
        session_id: &str,
        turn_id: &str,
        boundary: MessageBoundary,
    ) -> Result<()>;
    /// Get the TODO list for a session.
    async fn get_todo(&self, session_id: &str) -> Result<Option<String>>;
    /// Set the TODO list for a session.
    async fn set_todo(&self, session_id: &str, content: &str) -> Result<()>;

    // ── Session events (#1108 P1b/P2a) ─────────────────────────────

    /// Append a non-message engine event to the session's debug trail.
    ///
    /// `kind` should be one of [`session_event_kind`]. `payload` is
    /// kind-specific (see those constants). `parent_tool_call_id` is
    /// `Some(call_id)` only for sub-agent events so the renderer can
    /// fold them under the parent's `InvokeAgent` tool result.
    ///
    /// Hot-path: called from every `EngineEvent::Info`/`ChildTaskUpdate`
    /// emission via the engine's `PersistingSink`. Failures
    /// are logged but never propagated — a DB hiccup must not crash
    /// the inference loop.
    async fn insert_session_event(
        &self,
        session_id: &str,
        kind: &str,
        payload: &str,
        parent_tool_call_id: Option<&str>,
    ) -> Result<i64>;

    /// Load all session events in append order (by `id`).
    ///
    /// Used by the transcript renderer to interleave events with
    /// messages. Returns an empty Vec for sessions with no events.
    async fn load_session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Role::as_str ──────────────────────────────────────────────────────

    #[test]
    fn test_role_as_str_all_variants() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::Tool.as_str(), "tool");
    }

    // ── Role FromStr ──────────────────────────────────────────────────────

    #[test]
    fn test_role_from_str_round_trips() {
        for (s, expected) in [
            ("system", Role::System),
            ("user", Role::User),
            ("assistant", Role::Assistant),
            ("tool", Role::Tool),
        ] {
            let parsed: Role = s.parse().expect(s);
            assert_eq!(parsed.as_str(), expected.as_str());
        }
    }

    #[test]
    fn test_role_from_str_unknown_returns_error() {
        let result: Result<Role, _> = "unknown".parse();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown role"));
    }

    // ── Role Display ──────────────────────────────────────────────────────

    #[test]
    fn test_role_display_matches_as_str() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            assert_eq!(role.to_string(), role.as_str());
        }
    }
}
