//! Tool registry and execution engine.
//!
//! Each tool is a function that takes JSON arguments and returns a string result.
//! Path validation is enforced here to prevent directory traversal.
//!
//! ## Available tools
//!
//! | Tool | Module | Effect | Description |
//! |---|---|---|---|
//! | **Read** | `file_tools` | ReadOnly | Read file contents with line numbers |
//! | **Write** | `file_tools` | LocalMutation | Create or overwrite a file |
//! | **Edit** | `file_tools` | LocalMutation | Find-and-replace in an existing file |
//! | **Delete** | `file_tools` | Destructive | Delete a file |
//! | **List** | `file_tools` | ReadOnly | List files and directories |
//! | **Bash** | `shell` | LocalMutation | Execute shell commands (with background mode) |
//! | **Grep** | `grep` | ReadOnly | Recursive text search (respects .gitignore) |
//! | **Glob** | `glob_tool` | ReadOnly | Find files by glob pattern |
//! | **WebFetch** | `web_fetch` | RemoteAction | Fetch URL content (HTML→text) |
//! | **WebSearch** | `web_search` | RemoteAction | Web search via DuckDuckGo |
//! | **InvokeAgent** | `agent` | LocalMutation | Delegate task to a sub-agent |
//! | **ListAgents** | `agent` | ReadOnly | List available sub-agents |
//! | **MemoryRead** | `memory` | ReadOnly | Read project/global memory |
//! | **MemoryWrite** | `memory` | LocalMutation | Save facts to memory |
//! | **TodoWrite** | `todo` | ReadOnly | Update session task list (no FS impact) |
//! | **AskUser** | `ask_user` | ReadOnly | Ask the user a question |
//! | **ActivateSkill** | `skills` | ReadOnly | Load a skill's instructions |
//! | **ListSkills** | `skills` | ReadOnly | List available skills |
//! | **ListBackgroundTasks** | `bg_task_tools` | ReadOnly | Snapshot background tasks owned by the caller |
//! | **CancelTask** | `bg_task_tools` | ReadOnly | Cancel a background agent or process |
//! | **WaitTask** | `bg_task_tools` | ReadOnly | Block until a background task finishes (max 300 s) |
//!
//! ## Safety model
//!
//! Every tool call is classified by `ToolEffect` and checked against the
//! current approval mode before execution. See
//! [`crate::tools::ToolCatalog::classify_call`] for the per-call entry point and
//! each tool's `Tool::classify` impl for the actual logic.

/// Effect classification for tool calls.
///
/// Two-axis model: what does the tool touch (local vs. remote)
/// and how severe are its effects (read vs. mutate vs. destroy)?
///
/// # Examples
///
/// ```
/// use koda_core::tools::ToolEffect;
///
/// assert!(!ToolEffect::ReadOnly.is_mutating());
/// assert!(ToolEffect::LocalMutation.is_mutating());
/// assert!(ToolEffect::Destructive.is_mutating());
/// assert!(ToolEffect::RemoteAction.is_mutating());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ToolEffect {
    /// No side-effects: file reads, grep, git status.
    ReadOnly,
    /// Side-effects on remote services only: GitHub API, WebFetch POST.
    RemoteAction,
    /// Mutates local filesystem or state: Write, Edit, Delete, MemoryWrite.
    LocalMutation,
    /// Irreversible or high-blast-radius: rm -rf, git push --force, DROP TABLE.
    Destructive,
}

impl ToolEffect {
    /// `true` for any effect class **other than** `ReadOnly`.
    ///
    /// Replaces the legacy free function `tools::is_mutating_tool`
    /// (deleted in PR-9 of #1265 item 5). Lifting it onto the enum
    /// puts the predicate next to the variants it inspects, so any
    /// future class addition forces the author to think about which
    /// side of this fence it belongs on.
    #[inline]
    pub fn is_mutating(self) -> bool {
        !matches!(self, ToolEffect::ReadOnly)
    }
}

/// Classify a built-in tool by name.
///
/// Sub-agent invocation tool (`InvokeAgent`, `ListAgents`).
pub mod agent;
pub mod ask_user;
pub mod bg_process;
/// Background-task management tools — `ListBackgroundTasks`,
/// `CancelTask`, `WaitTask` (Layer 2 of #996).
pub mod bg_task_tools;
/// Read-only tool metadata catalog (#1265 item 5, PR-1/N).
/// Owns built-in definitions + the MCP manager slot. `ToolRegistry`
/// composes one of these and delegates the read-only methods.
pub mod catalog;
pub use catalog::ToolCatalog;
/// `Tool` trait + `ToolExecCtx` (#1265 item 5, PR-3/N). The seam for
/// the per-tool migration that follows in PR-4..PR-N. Each migrated
/// tool becomes a unit struct implementing this trait, replacing its
/// arms in the `classify_tool` / `execute()` / `is_mutating_tool` /
/// `extract_file_path` matches with co-located methods.
pub mod tool_trait;
pub use tool_trait::{DynTool, Tool, ToolExecCtx, boxed};
/// File CRUD tools (`Read`, `Write`, `Edit`, `Delete`, `List`).
pub mod file_tools;
pub mod fuzzy;
/// Glob pattern search tool (`Glob`).
pub mod glob_tool;
/// Recursive text search tool (`Grep`).
pub mod grep;
/// Project memory read/write tools (`MemoryRead`, `MemoryWrite`).
pub mod memory;
/// On-demand conversation history retrieval (`RecallContext`).
pub mod recall;
pub mod send_message;
/// Shell command execution tool (`Bash`).
pub mod shell;
/// Skill discovery and activation tools (`ListSkills`, `ActivateSkill`).
pub mod skill_tools;
/// Renderer-agnostic tool-call display payload (the single source of
/// truth for "what does a tool call show?"). See [`summary`] for the
/// drift problem this exists to solve.
pub mod summary;
/// Session-scoped task list tool (`TodoWrite`).
pub mod todo;
/// Pre-flight validation for tool calls (runs before approval).
pub mod validate;
pub mod wait_for_mail;
/// HTTP fetch tool (`WebFetch`).
pub mod web_fetch;
/// Web search tool (`WebSearch`).
pub mod web_search;

use anyhow::Result;
use koda_sandbox::fs::{FileSystem, LocalFileSystem};
use path_clean::PathClean;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::output_caps::OutputCaps;

use crate::providers::ToolDefinition;

/// Shared file-read cache: tracks `(size, mtime, sha256_hex)` per cache key.
///
/// The SHA-256 field is populated on full-file reads and used by `edit_file`
/// to detect whether the file changed between when the model last read it and
/// when it attempts an edit (Gemini CLI strategy, better than mtime-only because
/// mtime has 1-second granularity and can miss sub-second bash mutations).
///
/// `sha256_hex` is empty for line-range reads where only a slice was fetched.
///
/// Wrapped in `Arc` so parent and sub-agent `ToolRegistry` instances
/// share the same cache — reads by one agent benefit all others.
pub type FileReadCache = Arc<std::sync::Mutex<HashMap<String, (u64, SystemTime, String)>>>;

/// Tracks which tool last wrote each absolute file path.
///
/// Keyed by canonical `PathBuf`; value is `(tool_name, when)` using a
/// monotonic `Instant`. Populated on every successful Write and Edit so
/// the validation layer can include the responsible tool in staleness
/// error messages (#804 item 7).
pub type LastWriterCache = Arc<std::sync::Mutex<HashMap<PathBuf, (String, std::time::Instant)>>>;

/// Tracks the most recent successful Bash invocation.
///
/// Stores `(command_snippet, when)`. Only the latest call is kept — enough
/// context to tell the model "Bash ran 2s ago, it may have changed the file".
pub type LastBashCache = Arc<std::sync::Mutex<Option<(String, std::time::Instant)>>>;

/// Result of executing a tool.
///
/// The `success` field is set automatically by `ToolRegistry::execute()` —
/// `Ok(…)` → `true`, `Err(…)` → `false`. Individual tool functions just
/// return `Result<String>`.
///
/// ```
/// use koda_core::tools::ToolResult;
///
/// let ok = ToolResult { output: "done".into(), success: true, full_output: None };
/// assert!(ok.success);
/// ```
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// The tool's output string (model-facing; may be a summary for Bash).
    pub output: String,
    /// Whether the tool executed successfully.
    ///
    /// Set automatically by `ToolRegistry::execute()` — `Ok(…)` → `true`,
    /// `Err(…)` → `false`. Individual tools never set this directly;
    /// they just return `Result<String>`.
    pub success: bool,
    /// Full untruncated output, stored separately in DB for later retrieval.
    ///
    /// Only populated by Bash when output exceeds the summary threshold.
    /// `RecallContext` can search this to retrieve details the model didn't
    /// see in its context window.
    pub full_output: Option<String>,
}

/// Convert a `Result<String>` (the legacy free-function return shape)
/// into a `ToolResult` with consistent error formatting.
///
/// Used by `Tool` trait implementations that delegate to existing free
/// functions returning `Result<String>` (PR-4..PR-N migrations of
/// #1265 item 5). The error formatting (`{:#}` walks the anyhow
/// context chain per #1232 §4) matches the centralized post-match
/// converter in `ToolRegistry::execute` byte-for-byte — no behavior
/// drift between migrated and not-yet-migrated tools.
///
/// `full_output` is always `None` here; tools that produce truncated
/// output (currently only `Bash`) populate it themselves by
/// constructing `ToolResult` directly.
pub fn wrap_result(r: anyhow::Result<String>) -> ToolResult {
    match r {
        Ok(output) => ToolResult {
            output,
            success: true,
            full_output: None,
        },
        Err(e) => ToolResult {
            output: format!("Error: {e:#}"),
            success: false,
            full_output: None,
        },
    }
}

/// The tool registry: maps tool names to their definitions and handlers.
pub struct ToolRegistry {
    project_root: PathBuf,
    /// Read-only tool metadata — built-in definitions and the MCP
    /// manager slot. Extracted into [`ToolCatalog`] in #1265 item 5
    /// PR-1; `ToolRegistry` delegates `get_definitions`,
    /// `all_builtin_tool_names`, `has_tool`, `classify_tool_with_mcp`,
    /// `set_mcp_manager`, and `mcp_manager` to it. Pre-#1265 these
    /// were two separate fields (`definitions: HashMap<...>` and
    /// `mcp_manager: RwLock<Option<...>>`) on this struct.
    catalog: ToolCatalog,
    read_cache: FileReadCache,
    /// Filesystem abstraction — `LocalFileSystem` by default; swap to
    /// `SandboxedFileSystem` when a sandbox slot is active (Phase 2d, #934).
    /// Explicit `+ Send + Sync` is required: trait objects don't
    /// auto-inherit auto-traits from the supertrait, so without these
    /// bounds `ToolRegistry` becomes `!Send` and any future holding
    /// it (e.g. `execute_sub_agent`) cannot be `tokio::spawn`'d.
    fs: Arc<dyn FileSystem + Send + Sync>,
    /// Per-file last-writer tracking for richer staleness errors (#804 item 7).
    last_writer: LastWriterCache,
    /// Most recent Bash invocation for staleness error context (#804 item 7).
    last_bash: LastBashCache,
    /// Undo stack for file mutations.
    pub undo: std::sync::Mutex<crate::undo::UndoStack>,
    /// Discovered skills.
    pub skill_registry: crate::skills::SkillRegistry,
    /// Database handle for tools that need session access (RecallContext).
    db: std::sync::RwLock<Option<std::sync::Arc<crate::db::Database>>>,
    /// Current session ID (for RecallContext).
    session_id: std::sync::RwLock<Option<String>>,
    /// Context-scaled output caps for all tools.
    pub caps: OutputCaps,
    /// Background process registry — tracks processes spawned with `background: true`.
    /// Dropped (SIGTERM all) when the session ends.
    pub bg_registry: bg_process::BgRegistry,
    /// Trust mode — determines sandbox configuration for Bash tool.
    trust: crate::trust::TrustMode,
    /// Active sandbox policy. Phase 5 PR-2 of #934 wires this through
    /// the Bash dispatch path so per-agent variation becomes possible.
    /// Today every constructor seeds it with `SandboxPolicy::strict_default()`
    /// so behavior is byte-for-byte unchanged — PR-3 starts populating it
    /// with non-default values via [`crate::sandbox::policy_for_agent`].
    sandbox_policy: koda_sandbox::SandboxPolicy,
    /// Loopback port of the per-session HTTP CONNECT proxy (Phase 3b of
    /// #934). When `Some`, [`crate::sandbox::build`] attaches the
    /// canonical `HTTPS_PROXY`/`NO_PROXY`/etc. env-var bouquet to every
    /// Bash invocation so child processes route HTTP through the proxy.
    /// `None` (default) preserves the pre-3b unfiltered behavior —
    /// session code opts in by calling [`Self::set_proxy_port`].
    proxy_port: std::sync::RwLock<Option<u16>>,
    /// Loopback port of the per-session SOCKS5 proxy (Phase 3d.1 of
    /// #934). When `Some`, [`crate::sandbox::build`] appends
    /// `ALL_PROXY=socks5h://127.0.0.1:port` (+ lowercase alias) so
    /// raw-TCP clients (git over ssh, gRPC) that ignore `HTTPS_PROXY`
    /// also route through the hostname-filtered proxy. Independent
    /// from `proxy_port` so tests can attach one without the other.
    socks5_port: std::sync::RwLock<Option<u16>>,
    /// Phase 3 of #1325 — per-session path → mailbox lookup. Set
    /// by [`crate::session::KodaSession::new`] after constructing
    /// the registry. `None` until that wiring runs (and stays `None`
    /// in standalone-`ToolRegistry` tests where there's no session).
    /// Read by the `send_message` and `wait_for_mail` peer tools —
    /// every other tool ignores it.
    mailbox_registry:
        std::sync::RwLock<Option<Arc<crate::agent::mailbox_registry::MailboxRegistry>>>,
}

impl ToolRegistry {
    /// Create a new registry with all built-in tools.
    ///
    /// `max_context_tokens` scales all output caps (see `OutputCaps`).
    pub fn new(project_root: PathBuf, max_context_tokens: usize) -> Self {
        Self::with_trust(
            project_root,
            max_context_tokens,
            crate::trust::TrustMode::Safe,
        )
    }

    /// Create a new registry with a specific trust mode.
    pub fn with_trust(
        project_root: PathBuf,
        max_context_tokens: usize,
        trust: crate::trust::TrustMode,
    ) -> Self {
        // Built-in tool definitions and the MCP manager slot moved
        // into [`ToolCatalog`] in #1265 item 5 PR-1. Construction is
        // a single call now — the per-tool `definitions()` walk
        // lives inside `ToolCatalog::new()`.
        let catalog = ToolCatalog::new();
        let skill_registry = crate::skills::SkillRegistry::discover(&project_root);

        Self {
            project_root,
            catalog,
            read_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            fs: Arc::new(LocalFileSystem::new()),
            last_writer: Arc::new(std::sync::Mutex::new(HashMap::new())),
            last_bash: Arc::new(std::sync::Mutex::new(None)),
            undo: std::sync::Mutex::new(crate::undo::UndoStack::new()),
            skill_registry,
            db: std::sync::RwLock::new(None),
            session_id: std::sync::RwLock::new(None),
            caps: OutputCaps::for_context(max_context_tokens),
            bg_registry: bg_process::BgRegistry::new(),
            trust,
            // Phase 5 PR-2 of #934: seed with strict_default(). Callers
            // can override via [`Self::with_sandbox_policy`] (sub-agent
            // dispatch does this; the main agent inherits the default).
            sandbox_policy: koda_sandbox::SandboxPolicy::strict_default(),
            proxy_port: std::sync::RwLock::new(None),
            socks5_port: std::sync::RwLock::new(None),
            mailbox_registry: std::sync::RwLock::new(None),
        }
    }

    /// Share an existing file-read cache (e.g. from the parent agent).
    ///
    /// Sub-agents that share the parent's cache avoid redundant disk reads
    /// for files already loaded in the same session.
    pub fn with_shared_cache(mut self, cache: FileReadCache) -> Self {
        self.read_cache = cache;
        self
    }

    /// Override the active sandbox policy.
    ///
    /// Phase 5 PR-2 of #934. Builder-style; chains after `with_trust`
    /// (or `new`). Sub-agent dispatch uses this to install the policy
    /// produced by [`crate::sandbox::policy_for_agent`] on the child's
    /// registry. The main agent path doesn't call this and inherits
    /// the `strict_default()` seed from `with_trust` — byte-for-byte
    /// unchanged behavior in PR-2.
    pub fn with_sandbox_policy(mut self, policy: koda_sandbox::SandboxPolicy) -> Self {
        self.sandbox_policy = policy;
        self
    }

    /// Borrow the active sandbox policy. Used by the Bash dispatch
    /// path to thread the per-registry policy into
    /// [`crate::sandbox::build`].
    pub fn sandbox_policy(&self) -> &koda_sandbox::SandboxPolicy {
        &self.sandbox_policy
    }

    /// Inject a custom [`FileSystem`] implementation.
    ///
    /// Call this after construction to swap `LocalFileSystem` for
    /// `SandboxedFileSystem` when a sandbox slot is ready (#934).
    pub fn set_fs(&mut self, fs: Arc<dyn FileSystem + Send + Sync>) {
        self.fs = fs;
    }

    /// Get a clone of the `Arc` file-read cache for sharing with sub-agents.
    pub fn file_read_cache(&self) -> FileReadCache {
        Arc::clone(&self.read_cache)
    }

    /// Get a clone of the last-writer cache for passing to validation.
    pub fn last_writer_cache(&self) -> LastWriterCache {
        Arc::clone(&self.last_writer)
    }

    /// Get a clone of the last-bash cache for passing to validation.
    pub fn last_bash_cache(&self) -> LastBashCache {
        Arc::clone(&self.last_bash)
    }

    /// Attach database + session for tools that need history access.
    pub fn set_session(&self, db: std::sync::Arc<crate::db::Database>, session_id: String) {
        if let Ok(mut guard) = self.db.write() {
            *guard = Some(db);
        }
        if let Ok(mut guard) = self.session_id.write() {
            *guard = Some(session_id);
        }
    }

    /// Attach an MCP connection manager and register its tools (#662).
    ///
    /// Called after MCP servers have connected and discovered their tools.
    /// Tool definitions are merged into the registry so the LLM can see them.
    ///
    /// Delegates to [`ToolCatalog::set_mcp_manager`] (#1265 item 5 PR-1).
    pub fn set_mcp_manager(&self, manager: Arc<tokio::sync::RwLock<crate::mcp::McpManager>>) {
        self.catalog.set_mcp_manager(manager);
    }

    /// Get the MCP manager (if attached). Delegates to
    /// [`ToolCatalog::mcp_manager`] (#1265 item 5 PR-1).
    pub fn mcp_manager(&self) -> Option<Arc<tokio::sync::RwLock<crate::mcp::McpManager>>> {
        self.catalog.mcp_manager()
    }

    /// Attach (or detach) the per-session HTTP CONNECT proxy port.
    ///
    /// Called from [`crate::session::KodaSession::new`] after spawning
    /// the always-on [`koda_sandbox::BuiltInProxy`]. Pass `None` to
    /// detach (Bash invocations revert to unfiltered network access —
    /// only used in standalone-ToolRegistry tests; production sessions
    /// keep this set for their full lifetime). Lock-poisoning is
    /// non-fatal — we silently keep the previous value, matching the
    /// precedent set by `set_mcp_manager`.
    pub fn set_proxy_port(&self, port: Option<u16>) {
        if let Ok(mut guard) = self.proxy_port.write() {
            *guard = port;
        }
    }

    /// Current proxy port, if one has been attached. Read by the Bash
    /// dispatch path; threaded into [`crate::sandbox::build`] which
    /// turns it into the env-var bouquet on the spawned `Command`.
    pub fn proxy_port(&self) -> Option<u16> {
        self.proxy_port.read().ok().and_then(|guard| *guard)
    }

    /// Attach (or detach) the per-session SOCKS5 proxy port. Mirrors
    /// [`Self::set_proxy_port`] — see that fn's docs for the
    /// lock-poisoning policy.
    pub fn set_socks5_port(&self, port: Option<u16>) {
        if let Ok(mut guard) = self.socks5_port.write() {
            *guard = port;
        }
    }

    /// Current SOCKS5 port, if one has been attached. Threaded into
    /// [`crate::sandbox::build`] which appends `ALL_PROXY` to the
    /// spawned `Command`'s env.
    pub fn socks5_port(&self) -> Option<u16> {
        self.socks5_port.read().ok().and_then(|guard| *guard)
    }

    /// Attach the per-session mailbox registry (#1325 Phase 3).
    ///
    /// Called once from [`crate::session::KodaSession::new`] right
    /// after the registry is constructed and the root path is
    /// pre-registered. Read by the `send_message` and `wait_for_mail`
    /// peer tools — every other tool ignores it.
    ///
    /// Lock-poisoning silently keeps the previous value (matching
    /// the precedent set by `set_proxy_port` / `set_session`). If
    /// poisoning ever happens here in production it'd mean a
    /// concurrent panic during session construction — the session
    /// is already toast and there's no useful recovery.
    pub fn set_mailbox_registry(
        &self,
        registry: Arc<crate::agent::mailbox_registry::MailboxRegistry>,
    ) {
        if let Ok(mut guard) = self.mailbox_registry.write() {
            *guard = Some(registry);
        }
    }

    /// Current mailbox registry, if one has been attached. Returns
    /// the `Arc` clone so the caller can hold it across `.await`
    /// boundaries without holding the registry-slot lock.
    pub fn mailbox_registry(&self) -> Option<Arc<crate::agent::mailbox_registry::MailboxRegistry>> {
        self.mailbox_registry.read().ok().and_then(|g| g.clone())
    }

    /// Borrow the underlying read-only metadata catalog.
    ///
    /// Use this for per-call classification
    /// ([`ToolCatalog::classify_call`]) and per-tool undo-path
    /// resolution ([`ToolCatalog::get_tool`]) — both replaced the
    /// legacy `tools::classify_tool` / `undo::extract_file_path`
    /// free functions in PR-9 of #1265 item 5.
    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    /// Classify a tool using MCP annotations when available.
    ///
    /// Delegates to [`ToolCatalog::classify_tool_with_mcp`] (#1265 item 5 PR-1).
    pub fn classify_tool_with_mcp(&self, name: &str) -> ToolEffect {
        self.catalog.classify_tool_with_mcp(name)
    }

    /// Get all built-in tool names.
    /// Used by wiring tests to verify every tool is properly integrated.
    ///
    /// Delegates to [`ToolCatalog::all_builtin_tool_names`] (#1265 item 5 PR-1).
    pub fn all_builtin_tool_names(&self) -> Vec<String> {
        self.catalog.all_builtin_tool_names()
    }

    /// Check whether a tool name is known.
    ///
    /// Delegates to [`ToolCatalog::has_tool`] (#1265 item 5 PR-1).
    pub fn has_tool(&self, name: &str) -> bool {
        self.catalog.has_tool(name)
    }

    /// List all available skills as `(name, description, source)` tuples.
    pub fn list_skills(&self) -> Vec<(String, String, String)> {
        self.skill_registry
            .list()
            .into_iter()
            .map(|m| {
                let source = match m.source {
                    crate::skills::SkillSource::BuiltIn => "built-in",
                    crate::skills::SkillSource::User => "user",
                    crate::skills::SkillSource::Project => "project",
                };
                (m.name.clone(), m.description.clone(), source.to_string())
            })
            .collect()
    }

    /// Search skills by query, returning `(name, description, source)` tuples.
    pub fn search_skills(&self, query: &str) -> Vec<(String, String, String)> {
        self.skill_registry
            .search(query)
            .into_iter()
            .map(|m| {
                let source = match m.source {
                    crate::skills::SkillSource::BuiltIn => "built-in",
                    crate::skills::SkillSource::User => "user",
                    crate::skills::SkillSource::Project => "project",
                };
                (m.name.clone(), m.description.clone(), source.to_string())
            })
            .collect()
    }

    /// Get tool definitions, optionally filtered by allow/deny lists.
    ///
    /// Includes MCP tool definitions if a manager is attached.
    ///
    /// - `allowed` non-empty → only those tools (allowlist).
    /// - `denied` non-empty → all tools except those (denylist).
    /// - Both empty → all tools.
    /// - If both are specified, allowlist wins (deny is ignored).
    ///
    /// Delegates to [`ToolCatalog::get_definitions`] (#1265 item 5 PR-1).
    pub fn get_definitions(&self, allowed: &[String], denied: &[String]) -> Vec<ToolDefinition> {
        self.catalog.get_definitions(allowed, denied)
    }

    /// Execute a tool by name with the given JSON arguments.
    ///
    /// Empty or whitespace-only `arguments` are treated as `{}` (no args)
    /// so that tools can fall through to their own defaults instead of
    /// surfacing a raw JSON parse error.  See #513.
    ///
    /// `sink_for_streaming` is an optional `(sink, call_id)` pair. When
    /// provided, the Bash tool streams each output line as a
    /// `ToolOutputLine` event in real-time.
    pub async fn execute(
        &self,
        name: &str,
        arguments: &str,
        sink_for_streaming: Option<(&dyn crate::engine::EngineSink, &str)>,
        // Phase E of #996: forwarded to `Bash` so that bg-shell
        // entries are tagged with the calling agent's invocation id.
        // Every other tool ignores this. Top-level callers pass `None`.
        caller_spawner: Option<u32>,
        // #1325 Phase 4: caller's identity in the agent spawn tree.
        // Stamped into `ToolExecCtx::caller_agent_path` so peer
        // tools can `author=`/look up `me` reliably. Top-level
        // (root) callers pass `&AgentPath::root()`.
        caller_agent_path: &crate::agent::AgentPath,
    ) -> ToolResult {
        let raw = arguments.trim();
        let raw = if raw.is_empty() { "{}" } else { raw };
        let args: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                return ToolResult {
                    output: format!("Invalid JSON arguments: {e}"),
                    success: false,
                    full_output: None,
                };
            }
        };

        tracing::info!(
            "Executing tool: {name} with args: [{} chars]",
            arguments.len()
        );

        // Snapshot file before mutation (for /undo).
        //
        // Every built-in tool is now on the `Tool` trait (#1265 item 5,
        // PR-4..PR-8) and owns its own undo behavior via
        // `Tool::extract_undo_path`. The trait's `None` default means
        // "don't snapshot". MCP tools (`server__tool` names) aren't
        // in the catalog, so they get `None` here — matching the
        // pre-#1265 behavior where MCP tools weren't in the legacy
        // `undo::is_mutating_tool` allowlist either.
        let undo_path = self
            .catalog
            .get_tool(name)
            .and_then(|tool| tool.extract_undo_path(&args));
        if let Some(file_path) = undo_path {
            let resolved = self.project_root.join(&file_path);
            if let Ok(mut undo) = self.undo.lock() {
                undo.snapshot(&resolved);
            }
        }

        // Trait dispatch for migrated tools (#1265 item 5, PR-4..PR-N).
        //
        // Hits this fast path for any tool registered in
        // `ToolCatalog::tools`; misses fall through to the legacy
        // `match` below. The branch is taken in `O(1)` (HashMap
        // lookup) so the cost on the miss path is negligible.
        //
        // Post-execution `Write`/`Edit` recording for the
        // file-tracker stays here — it's a registry-level concern
        // (it touches `self.last_writer`) and doesn't belong on the
        // per-tool struct. Same logic the legacy `match result`
        // block below applies; centralized here so it fires for
        // both migrated and not-yet-migrated tools.
        if let Some(tool) = self.catalog.get_tool(name) {
            // Trait-dispatch fast path. Reads sink + caller_spawner
            // from the call's parameters (not `None`) so trait-
            // migrated streaming tools (`Bash` from PR-6 onward)
            // still see them. The other fields come straight off
            // `self` — same shape every legacy match arm pre-#1265
            // would have read.
            let policy = self.sandbox_policy();
            let proxy_port = self.proxy_port();
            let socks5_port = self.socks5_port();
            // Snapshot the session pair under one lock-acquisition
            // each, hold the `Arc<Database>` so the borrow into the
            // ctx stays valid for the duration of `tool.execute`.
            let db_arc = self.db.read().ok().and_then(|g| g.clone());
            let sid_str = self.session_id.read().ok().and_then(|g| g.clone());
            let session = match (db_arc.as_deref(), sid_str.as_deref()) {
                (Some(db), Some(sid)) => Some((db, sid)),
                _ => None,
            };
            // Snapshot the mailbox-registry slot once per dispatch —
            // the read-guard is held only long enough to clone the
            // `Arc`, then dropped before the `.await` below. The
            // local `mb_reg_arc` keeps the `Arc` alive across the
            // `.await`; the borrow handed to the tool is `Option<&'_ Arc<_>>`.
            let mb_reg_arc = self.mailbox_registry.read().ok().and_then(|g| g.clone());
            let ctx = tool_trait::ToolExecCtx {
                project_root: &self.project_root,
                read_cache: &self.read_cache,
                fs: &*self.fs,
                caps: &self.caps,
                sink: sink_for_streaming,
                caller_spawner,
                bg_registry: &self.bg_registry,
                trust: &self.trust,
                sandbox_policy: policy,
                proxy_port,
                socks5_port,
                session,
                skill_registry: &self.skill_registry,
                mailbox_registry: mb_reg_arc.as_ref(),
                caller_agent_path,
            };
            let result = tool.execute(&ctx, &args).await;
            // Post-execution registry-level recording. Lives here
            // (not on the per-tool struct) because it touches
            // registry state. Each `name == "X"` arm is a sticky
            // hook bridging until the registry exposes a generic
            // post-exec hook (out of scope for this stack).
            if result.success {
                if matches!(name, "Write" | "Edit")
                    && let Some(path) =
                        crate::file_tracker::resolve_file_path_from_args(&args, &self.project_root)
                    && let Ok(mut guard) = self.last_writer.lock()
                {
                    guard.insert(path, (name.to_string(), std::time::Instant::now()));
                }
                if name == "Bash" {
                    // #804 item 7: record the most recent bash
                    // invocation snippet so the validation layer
                    // can name it in staleness errors.
                    let snippet = args["command"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(72)
                        .collect::<String>();
                    if !snippet.is_empty()
                        && let Ok(mut guard) = self.last_bash.lock()
                    {
                        *guard = Some((snippet, std::time::Instant::now()));
                    }
                }
            }
            return result;
        }

        // ----------------------------------------------------------
        // Fall-through: every built-in tool is now on the `Tool`
        // trait (PR-4 through PR-8 of #1265 item 5). The only paths
        // that reach here are MCP tool dispatch (`server__tool`
        // names) and the unknown-tool error reporter.
        //
        // Pre-#1265 the `match name { ... }` had ~16 arms ending in
        // a giant post-processing `match result { Ok|Err => ... }`
        // block that recorded Write/Edit timestamps and wrapped
        // results. All of that is now dead code (the trait fast
        // path above handles Write/Edit/Bash recording, and the
        // legacy fall-through never produces a path-tagged Ok
        // anymore). PR-9 cleanup deleted it.
        // ----------------------------------------------------------

        // MCP tool dispatch (#662): route `server__tool` calls to
        // the appropriate MCP server.
        if crate::mcp::is_mcp_tool_name(name) {
            if let Some(mgr) = self.mcp_manager() {
                let result = {
                    let mgr = mgr.read().await;
                    mgr.call_tool(name, args.clone()).await
                };
                return match result {
                    Ok(output) => ToolResult {
                        output,
                        success: true,
                        full_output: None,
                    },
                    Err(e) => ToolResult {
                        // #1232 §4: `{:#}` walks the anyhow context
                        // chain so the model sees the full cause.
                        output: format!("Error: {e:#}"),
                        success: false,
                        full_output: None,
                    },
                };
            }
            return ToolResult {
                output: format!(
                    "MCP tool '{name}' not available \u{2014} \
                     no MCP servers connected."
                ),
                success: false,
                full_output: None,
            };
        }

        // Detect garbled tool names (JSON blobs, very long strings)
        // — a sign the model can't do structured tool calling.
        let warning = if name.contains('{') || name.len() > 64 {
            format!(
                "Unknown tool: {name}. \
                 This model appears to struggle with tool calling. \
                 Consider switching to a model with native function-call support."
            )
        } else {
            format!("Unknown tool: {name}")
        };
        ToolResult {
            output: format!("Error: {warning}"),
            success: false,
            full_output: None,
        }
    }
}
/// Validate and resolve a path, preventing directory traversal.
///
/// Works for both existing and non-existing files (no `canonicalize!`).
/// Relative paths are joined to `project_root`; absolute paths must
/// still be within `project_root` **or** under an allowed tempdir
/// (`/tmp`, `/private/tmp`, `/var/tmp`, or `$TMPDIR`).
///
/// # Examples
///
/// ```
/// use koda_core::tools::safe_resolve_path;
/// use std::path::Path;
///
/// let root = Path::new("/home/user/project");
///
/// // Relative paths resolve within project
/// let p = safe_resolve_path(root, "src/main.rs").unwrap();
/// assert_eq!(p, Path::new("/home/user/project/src/main.rs"));
///
/// // Traversal is blocked
/// assert!(safe_resolve_path(root, "../../etc/passwd").is_err());
///
/// // Tempdirs are allowed (matches the kernel sandbox policy)
/// assert!(safe_resolve_path(root, "/tmp/scratch.txt").is_ok());
/// ```
pub fn safe_resolve_path(project_root: &Path, requested: &str) -> Result<PathBuf> {
    // NOTE: used only for Write / Edit / Delete.  Read-only tools call
    // resolve_path_unrestricted — see docs/src/sandbox.md for the rationale.
    let requested_path = Path::new(requested);

    // Build absolute path and normalize (removes .., . etc.)
    let resolved = if requested_path.is_absolute() {
        requested_path.to_path_buf().clean()
    } else {
        project_root.join(requested_path).clean()
    };

    // Security check: must be within project root OR an allowed tempdir.
    // Only Write / Edit / Delete are gated here — reads are unrestricted
    // (see resolve_path_unrestricted and docs/src/sandbox.md).
    //
    // The tempdir allow-list keeps in-process policy in sync with the
    // kernel sandbox (Seatbelt on macOS, bwrap on Linux), which already
    // permits writes to /tmp + cache dirs. Pre-fix this layer was the
    // outlier (#947): `bash -c 'cat > /tmp/x'` succeeded but `Write /tmp/x`
    // was rejected, blocking common scratch-file workflows.
    if !resolved.starts_with(project_root) && !is_allowed_write_root(&resolved) {
        anyhow::bail!(
            "Path {requested:?} is outside the project root ({project_root:?}) \
             and not under a writable tempdir (/tmp, /var/tmp, $TMPDIR). \
             Write, Edit, and Delete are restricted to the project directory \
             and tempdirs to prevent accidental modification of files \
             elsewhere. Tell the user: to write outside these locations, \
             restart koda from a parent directory that contains both paths."
        );
    }

    // Defense in depth: even within an allowed tempdir, never let writes
    // touch koda's own credential store. (`is_fully_denied` matches the
    // path against the credential-config denylist used by the read-only
    // tools, keeping all three perimeters — read, write, sandbox — in sync.)
    if crate::sandbox::is_fully_denied(&resolved) {
        anyhow::bail!(
            "Path {requested:?} is denied: this path contains koda's \
             internal secrets and cannot be modified by tool calls."
        );
    }

    Ok(resolved)
}

/// Returns true if `path` lives under a system tempdir that the kernel
/// sandbox (Seatbelt / bwrap) already permits writes to.
///
/// This intentionally mirrors the `(subpath "/tmp")` and
/// `(subpath "/private/tmp")` allow rules in `sandbox.rs` so the in-process
/// file tools accept the same set of paths as `bash -c 'cat > ...'`.
///
/// The check is logical (no `canonicalize`) to match `safe_resolve_path`'s
/// behaviour for non-existing files. The kernel sandbox is the real enforcer
/// at runtime; this helper is the policy-symmetry layer.
fn is_allowed_write_root(path: &Path) -> bool {
    // Hard-coded tempdir paths that the kernel sandbox always allows.
    // `/private/tmp` is macOS's realpath of `/tmp` (which is a symlink).
    const TEMPDIR_PREFIXES: &[&str] = &["/tmp", "/private/tmp", "/var/tmp"];
    if TEMPDIR_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return true;
    }

    // Per-user $TMPDIR (macOS: /var/folders/.../T/, Linux: usually /tmp).
    // Resolved at call time so test environments overriding TMPDIR
    // are honoured. `temp_dir()` is infallible — falls back to /tmp on Unix.
    path.starts_with(std::env::temp_dir())
}

/// Return the policy list of roots a mutation tool (Write/Edit/Delete) is
/// allowed to touch.
///
/// Single source of truth shared between [`safe_resolve_path`] (logical
/// check, runs first) and `koda_sandbox::fs::verify_mutation_safe`
/// (canonicalizing symlink check, runs second). When you add or remove a
/// writable root, change this function and both checks pick up the new
/// policy automatically. See #1281.
///
/// The returned roots are *not* canonicalized — that's the verifier's
/// job. Some roots (e.g. per-user `$TMPDIR` on a stripped-down container)
/// may not exist on disk; the verifier silently skips missing ones.
pub fn allowed_mutation_roots(project_root: &Path) -> Vec<PathBuf> {
    // Mirrors is_allowed_write_root + project_root. Order doesn't matter
    // for the verifier (it tries each root), but we put project_root
    // first so any error message that prints the list reads naturally.
    let mut roots = vec![
        project_root.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/tmp"),
        std::env::temp_dir(),
    ];
    // De-dupe so canonicalize doesn't get called multiple times for the
    // same physical root on Linux where /tmp == $TMPDIR.
    roots.sort();
    roots.dedup();
    roots
}

/// Normalise a path without enforcing any scope restriction.
///
/// Low-level primitive — **tool implementations should call
/// [`resolve_read_path`] instead**, which adds the fully-denied list check
/// that keeps in-process policy in sync with the subprocess sandbox.
///
/// Relative paths are resolved against `project_root`; absolute paths are
/// cleaned in-place.  The result may point anywhere on the filesystem.
pub(crate) fn resolve_path_unrestricted(project_root: &Path, requested: &str) -> PathBuf {
    let path = Path::new(requested);
    if path.is_absolute() {
        path.to_path_buf().clean()
    } else {
        project_root.join(path).clean()
    }
}

/// Normalise a read-only path and enforce the fully-denied list.
///
/// This is the entry-point for **all read-only tools** (Read, List, Grep,
/// Glob).  It wraps `resolve_path_unrestricted` with a check against
/// `sandbox::is_fully_denied` so that the same paths blocked by the
/// subprocess sandbox (bwrap / Seatbelt) are also blocked when the model
/// accesses them through in-process tools.
///
/// Currently the only denied path is `~/.config/koda/db` — koda's own SQLite
/// database containing plaintext API keys.  Ordinary credential directories
/// (`~/.ssh`, `~/.aws`, …) are readable, matching the Bash sandbox policy.
///
/// See issue #884 for Option B (OS-level enforcement via sandboxed worker).
pub fn resolve_read_path(project_root: &Path, requested: &str) -> Result<PathBuf> {
    let resolved = resolve_path_unrestricted(project_root, requested);
    if crate::sandbox::is_fully_denied(&resolved) {
        anyhow::bail!(
            "Access to {requested:?} is denied: this path contains koda's \
             internal secrets and cannot be read by model tool calls."
        );
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    // ── Phase 3b: proxy port wiring (Bash → sandbox::build) ──────────

    #[test]
    fn proxy_port_defaults_to_none() {
        // Standalone ToolRegistry (no KodaSession) starts with no port —
        // production sessions overwrite this in `KodaSession::new`.
        let registry = ToolRegistry::new(root(), 100_000);
        assert_eq!(registry.proxy_port(), None);
    }

    #[test]
    fn proxy_port_round_trips_through_setter() {
        let registry = ToolRegistry::new(root(), 100_000);
        registry.set_proxy_port(Some(31415));
        assert_eq!(registry.proxy_port(), Some(31415));
    }

    // ── Phase 3d.2: SOCKS5 port wiring (Bash → sandbox::build) ───────

    #[test]
    fn socks5_port_defaults_to_none() {
        let registry = ToolRegistry::new(root(), 100_000);
        assert_eq!(registry.socks5_port(), None);
    }

    #[test]
    fn socks5_port_round_trips_through_setter() {
        let registry = ToolRegistry::new(root(), 100_000);
        registry.set_socks5_port(Some(27182));
        assert_eq!(registry.socks5_port(), Some(27182));
    }

    #[test]
    fn socks5_and_http_ports_are_independent() {
        // Setting one must not clobber the other — the two proxies are
        // spawned independently and may live or die independently.
        let registry = ToolRegistry::new(root(), 100_000);
        registry.set_proxy_port(Some(8080));
        registry.set_socks5_port(Some(1080));
        assert_eq!(registry.proxy_port(), Some(8080));
        assert_eq!(registry.socks5_port(), Some(1080));
        registry.set_proxy_port(None);
        assert_eq!(registry.socks5_port(), Some(1080));
    }

    #[test]
    fn test_relative_path_resolves_inside_root() {
        let result = safe_resolve_path(&root(), "src/main.rs").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn test_dot_path_resolves_to_root() {
        let result = safe_resolve_path(&root(), ".").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn test_new_file_in_new_dir_resolves() {
        let result = safe_resolve_path(&root(), "src/brand_new/feature.rs").unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/user/project/src/brand_new/feature.rs")
        );
    }

    #[test]
    fn test_dotdot_traversal_blocked() {
        let result = safe_resolve_path(&root(), "../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_dotdot_sneaky_traversal_blocked() {
        let result = safe_resolve_path(&root(), "src/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_absolute_path_inside_root_allowed() {
        let result = safe_resolve_path(&root(), "/home/user/project/src/lib.rs").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project/src/lib.rs"));
    }

    #[test]
    fn test_absolute_path_outside_root_blocked() {
        let result = safe_resolve_path(&root(), "/etc/shadow");
        assert!(result.is_err());
    }

    #[test]
    fn test_outside_root_error_is_actionable_for_user() {
        let err = safe_resolve_path(&root(), "../../etc/passwd").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("outside the project root"),
            "error must say 'outside the project root'; got: {msg}"
        );
        assert!(
            msg.contains("Tell the user"),
            "error must direct model to surface this to the user; got: {msg}"
        );
        // Must NOT suggest Bash — that would bypass the file-tool safety layer.
        assert!(
            !msg.contains("Bash"),
            "error must not suggest Bash as a workaround; got: {msg}"
        );
    }

    #[test]
    fn test_empty_path_resolves_to_root() {
        let result = safe_resolve_path(&root(), "").unwrap();
        assert_eq!(result, PathBuf::from("/home/user/project"));
    }

    // ── resolve_read_path ──────────────────────────────────────────────────

    #[test]
    fn read_path_allows_project_file() {
        let p = resolve_read_path(&root(), "src/lib.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/user/project/src/lib.rs"));
    }

    #[test]
    fn read_path_allows_outside_project() {
        // Reads outside the project root are intentionally unrestricted.
        let p = resolve_read_path(&root(), "/etc/hosts").unwrap();
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn read_path_blocks_koda_db() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        let koda_db = format!("{home}/.config/koda/db/koda.db");
        let err = resolve_read_path(&root(), &koda_db).unwrap_err();
        assert!(
            err.to_string().contains("denied"),
            "expected 'denied' in error, got: {err}"
        );
    }

    // ── #947: writes to tempdirs ASCII─ART─ ─────────────────────────
    //
    // The kernel sandbox (Seatbelt / bwrap) explicitly permits writes to
    // /tmp + cache dirs.  Pre-fix, `safe_resolve_path` rejected absolute
    // paths outside `project_root`, so `bash -c 'cat > /tmp/x'` succeeded
    // but `Write /tmp/x` failed — forcing models into shell heredoc
    // workarounds that often quote-escape badly.  These tests lock in the
    // symmetry between the two perimeters.

    #[test]
    fn write_path_allows_tmp() {
        let p = safe_resolve_path(&root(), "/tmp/koda-scratch.txt").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/koda-scratch.txt"));
    }

    #[test]
    fn write_path_allows_private_tmp_macos_realpath() {
        // macOS resolves /tmp → /private/tmp via a symlink. Some tools (`find`,
        // `realpath`) emit the realpath form, so absolute paths beginning
        // with /private/tmp must also be accepted.
        let p = safe_resolve_path(&root(), "/private/tmp/koda-scratch.txt").unwrap();
        assert_eq!(p, PathBuf::from("/private/tmp/koda-scratch.txt"));
    }

    #[test]
    fn write_path_allows_var_tmp() {
        let p = safe_resolve_path(&root(), "/var/tmp/koda-scratch.txt").unwrap();
        assert_eq!(p, PathBuf::from("/var/tmp/koda-scratch.txt"));
    }

    #[test]
    fn write_path_allows_per_user_tmpdir() {
        // Whatever `std::env::temp_dir()` returns on this host — macOS gives
        // /var/folders/.../T/, Linux usually /tmp.  Either way it's writable.
        let tmpdir = std::env::temp_dir();
        let target = tmpdir.join("koda-scratch.txt");
        let p = safe_resolve_path(&root(), target.to_str().unwrap()).unwrap();
        assert_eq!(p, target.clean());
    }

    #[test]
    fn write_path_blocks_etc_hosts() {
        // System config dirs stay denied — only tempdirs are added.
        let err = safe_resolve_path(&root(), "/etc/hosts").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("outside the project root"),
            "system paths must still be rejected; got: {msg}"
        );
    }

    #[test]
    fn write_path_blocks_ssh_authorized_keys() {
        // Credential dirs in $HOME stay denied — they're outside both
        // project_root and any tempdir, so the existing perimeter holds.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        let target = format!("{home}/.ssh/authorized_keys");
        assert!(
            safe_resolve_path(&root(), &target).is_err(),
            "~/.ssh writes must remain blocked"
        );
    }

    #[test]
    fn write_path_blocks_koda_db_even_via_tmp_traversal() {
        // Defense in depth: even if a model crafts a path that lands in a
        // tempdir but cleans into koda's own credential store, `is_fully_denied`
        // catches it. Constructed path: `/tmp/../<home>/.config/koda/db/x`
        // cleans to `<home>/.config/koda/db/x` — NOT a tempdir, NOT project,
        // hits the standard "outside the project root" path. So this is
        // already covered by the primary check; this test pins it down.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        let target = format!("/tmp/../{home}/.config/koda/db/koda.db");
        assert!(
            safe_resolve_path(&root(), &target).is_err(),
            "traversal out of /tmp must not bypass the gate"
        );
    }

    #[test]
    fn write_path_traversal_inside_tmp_stays_in_tmp() {
        // /tmp/foo/../bar cleans to /tmp/bar — still in /tmp, still allowed.
        let p = safe_resolve_path(&root(), "/tmp/foo/../bar").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/bar"));
    }

    // ── #1077 Phase A: TodoWrite event-emission contract ─────────
    //
    // The dispatch arm in `execute()` must:
    // 1. emit `EngineEvent::TodoUpdate` with structured items+diff on
    //    accepted writes that change the persisted list;
    // 2. emit nothing on the dedup-nudge path (empty diff);
    // 3. always return the model-facing message string regardless.
    //
    // These are the contract a future TUI / ACP renderer will rely on.
    // If you find yourself loosening any of them, revisit `DESIGN.md
    // § Progress Tracking: Model-Owned, History-Persisted,
    // Engine-Surfaced` first — the suppression rule in particular is
    // load-bearing for not spamming clients on idempotent rewrites.

    async fn registry_with_session() -> (
        ToolRegistry,
        tempfile::TempDir,
        std::sync::Arc<crate::db::Database>,
        String,
    ) {
        use crate::persistence::Persistence;
        let dir = tempfile::TempDir::new().unwrap();
        let db = std::sync::Arc::new(
            crate::db::Database::open(&dir.path().join("test.db"))
                .await
                .unwrap(),
        );
        let sid = db.create_session("koda", dir.path()).await.unwrap();
        let registry = ToolRegistry::new(dir.path().to_path_buf(), 100_000);
        // Wire DB + session id the same way KodaSession::new does.
        *registry.db.write().unwrap() = Some(db.clone());
        *registry.session_id.write().unwrap() = Some(sid.clone());
        (registry, dir, db, sid)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn todo_write_emits_todo_update_event_on_first_write() {
        let (registry, _dir, _db, _sid) = registry_with_session().await;
        let sink = crate::engine::sink::TestSink::new();
        let result = registry
            .execute(
                "TodoWrite",
                r#"{"todos":[{"content":"Add tests","status":"pending","priority":"high"}]}"#,
                Some((&sink, "call-1")),
                None,
                &crate::agent::AgentPath::root(),
            )
            .await;
        assert!(result.success, "first write must succeed: {result:?}");
        assert_eq!(sink.len(), 1, "first write must emit exactly one event");
        match &sink.events()[0] {
            crate::engine::EngineEvent::TodoUpdate { items, diff } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].content, "Add tests");
                assert_eq!(diff.added.len(), 1, "first write → everything in added");
                assert!(diff.changed.is_empty());
                assert!(diff.removed.is_empty());
            }
            other => panic!("expected TodoUpdate, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn todo_write_suppresses_event_on_unchanged_rewrite() {
        let (registry, _dir, _db, _sid) = registry_with_session().await;
        let payload = r#"{"todos":[{"content":"A","status":"pending","priority":"high"}]}"#;

        // First write: should emit.
        let sink1 = crate::engine::sink::TestSink::new();
        registry
            .execute("TodoWrite", payload, Some((&sink1, "c1")), None, &crate::agent::AgentPath::root())
            .await;
        assert_eq!(sink1.len(), 1);

        // Identical second write: must NOT emit. The dedup-nudge
        // message goes back to the model, but clients see nothing.
        let sink2 = crate::engine::sink::TestSink::new();
        let result2 = registry
            .execute("TodoWrite", payload, Some((&sink2, "c2")), None, &crate::agent::AgentPath::root())
            .await;
        assert!(result2.success);
        assert!(
            result2.output.contains("unchanged"),
            "model-facing message must still nudge: {}",
            result2.output
        );
        assert_eq!(
            sink2.len(),
            0,
            "unchanged rewrite must NOT emit a TodoUpdate event"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn todo_write_returns_model_message_even_without_sink() {
        // Production paths sometimes call `execute` with `None` for
        // the sink (top-level tool runs that aren't streaming). Must
        // still succeed and return the formatted message.
        let (registry, _dir, _db, _sid) = registry_with_session().await;
        let result = registry
            .execute(
                "TodoWrite",
                r#"{"todos":[{"content":"X","status":"pending","priority":"low"}]}"#,
                None,
                None,
                &crate::agent::AgentPath::root(),
            )
            .await;
        assert!(result.success);
        assert!(result.output.contains("0/1 done"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn todo_write_rejects_two_in_progress_at_dispatch() {
        // Engine-enforced single-in-progress: must surface as a
        // failed ToolResult, not a successful one with a warning.
        // Models notice failures more reliably than warnings.
        let (registry, _dir, _db, _sid) = registry_with_session().await;
        let sink = crate::engine::sink::TestSink::new();
        let result = registry
            .execute(
                "TodoWrite",
                r#"{"todos":[
                    {"content":"A","status":"in_progress","priority":"high"},
                    {"content":"B","status":"in_progress","priority":"medium"}
                ]}"#,
                Some((&sink, "c1")),
                None,
                &crate::agent::AgentPath::root(),
            )
            .await;
        assert!(
            !result.success,
            "two in_progress must produce a failed ToolResult"
        );
        assert!(
            result.output.contains("Only one task"),
            "failure message must explain the rule: {}",
            result.output
        );
        assert_eq!(sink.len(), 0, "failed validation must not emit an event");
    }
}

// ── Tool action descriptions ──────────────────────────────────

/// Generate a human-readable description of a tool action for approval prompts.
pub fn describe_action(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "Bash" => {
            let cmd = args
                .get("command")
                .or(args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let bg = args
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if bg {
                format!("[bg] {cmd}")
            } else {
                cmd.to_string()
            }
        }
        "Delete" => {
            let path = args
                .get("file_path")
                .or(args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if recursive {
                format!("Delete directory (recursive): {path}")
            } else {
                format!("Delete: {path}")
            }
        }
        "Write" => {
            let path = args
                .get("path")
                .or(args.get("file_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let overwrite = args
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if overwrite {
                format!("Overwrite file: {path}")
            } else {
                format!("Create file: {path}")
            }
        }
        "Edit" => {
            let path = if let Some(payload) = args.get("payload") {
                payload
                    .get("file_path")
                    .or(payload.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            } else {
                args.get("file_path")
                    .or(args.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
            };
            format!("Edit file: {path}")
        }
        "WebFetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Fetch URL: {url}")
        }
        "WebSearch" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Web search: {q}")
        }
        "TodoWrite" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Update todo list ({n} tasks)")
        }
        "MemoryWrite" => {
            let fact = args.get("fact").and_then(|v| v.as_str()).unwrap_or("?");
            let preview = if fact.len() > 60 {
                format!("{}…", &fact[..57])
            } else {
                fact.to_string()
            };
            format!("Save to memory: {preview}")
        }
        "SendMessage" => {
            // #1325 Phase 3: surface the recipient + a short content
            // preview so the user can decide whether to approve a
            // peer-mailbox effect without expanding the args. Mirrors
            // the preview-truncation pattern used by MemoryWrite.
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("?");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("?");
            let preview = if content.len() > 60 {
                format!("{}…", &content[..57])
            } else {
                content.to_string()
            };
            format!("Send message to {target}: {preview}")
        }
        _ => format!("Execute: {tool_name}"),
    }
}

#[cfg(test)]
mod describe_action_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_describe_bash() {
        let desc = describe_action("Bash", &json!({"command": "cargo build"}));
        assert!(desc.contains("cargo build"));
    }

    #[test]
    fn test_describe_delete() {
        let desc = describe_action("Delete", &json!({"file_path": "old.rs"}));
        assert!(desc.contains("old.rs"));
    }

    #[test]
    fn test_describe_edit() {
        let desc = describe_action("Edit", &json!({"payload": {"file_path": "src/main.rs"}}));
        assert!(desc.contains("src/main.rs"));
    }

    #[test]
    fn test_describe_write() {
        let desc = describe_action("Write", &json!({"path": "new.rs"}));
        assert!(desc.contains("Create file"));
        assert!(desc.contains("new.rs"));
    }

    #[test]
    fn test_describe_write_overwrite() {
        let desc = describe_action("Write", &json!({"path": "x.rs", "overwrite": true}));
        assert!(desc.contains("Overwrite"));
    }

    #[test]
    fn test_get_definitions_deny_list() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let denied = vec![
            "Write".to_string(),
            "Edit".to_string(),
            "Delete".to_string(),
        ];
        let defs = registry.get_definitions(&[], &denied);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Edit"));
        assert!(!names.contains(&"Delete"));
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Grep"));
    }

    #[test]
    fn test_get_definitions_allow_list_wins_over_deny() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let allowed = vec!["Read".to_string(), "Write".to_string()];
        let denied = vec!["Write".to_string()];
        // allow wins — Write should be present
        let defs = registry.get_definitions(&allowed, &denied);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Write"));
    }

    #[test]
    fn test_get_definitions_both_empty_returns_all() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        let all = registry.get_definitions(&[], &[]);
        assert!(all.len() > 10, "Should have many tools");
    }

    // ── Phase 5 PR-2 of #934: SandboxPolicy threading on ToolRegistry ──
    //
    // The Bash dispatch path now reads `self.sandbox_policy()` instead
    // of synthesizing `strict_default()` inline. These tests pin:
    //   1. The default seed is `strict_default()` so unchanged callers
    //      preserve byte-for-byte behavior.
    //   2. `with_sandbox_policy` actually replaces the field (the
    //      threading is real, not a stub).
    //   3. The accessor returns the most recent setter's value (no
    //      caching/aliasing surprises).

    #[test]
    fn registry_sandbox_policy_defaults_to_strict() {
        let registry = ToolRegistry::new(PathBuf::from("/tmp"), 128_000);
        assert_eq!(
            *registry.sandbox_policy(),
            koda_sandbox::SandboxPolicy::strict_default(),
            "PR-2 contract: ToolRegistry::new must seed strict_default() so \
             pre-PR callers see unchanged behavior"
        );
    }

    #[test]
    fn with_sandbox_policy_overrides_the_default() {
        // Build a deliberately-non-default policy by mutating one field.
        // We don't care which field — only that round-tripping through
        // `with_sandbox_policy` preserves the override and the default
        // would not match.
        let mut custom = koda_sandbox::SandboxPolicy::strict_default();
        custom
            .fs
            .allow_write
            .push(koda_sandbox::PathPattern::new("/pr2-marker"));

        let registry =
            ToolRegistry::new(PathBuf::from("/tmp"), 128_000).with_sandbox_policy(custom.clone());

        assert_eq!(
            *registry.sandbox_policy(),
            custom,
            "with_sandbox_policy must replace the field, not no-op"
        );
        assert_ne!(
            *registry.sandbox_policy(),
            koda_sandbox::SandboxPolicy::strict_default(),
            "sanity: the override is observably different from the default"
        );
    }
}
