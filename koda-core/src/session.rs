//! KodaSession — per-conversation state.
//!
//! Holds mutable, per-turn state: database handle, session ID,
//! provider instance, approval mode, and cancellation token.
//! Instantiable N times for parallel sub-agents or cowork mode.
//!
//! ## Architecture
//!
//! ```text
//! KodaAgent (shared, immutable)
//!   ├─ tools, system prompt, project root
//!   └─ shared via Arc across sessions
//!
//! KodaSession (per-conversation, mutable)
//!   ├─ database handle (SQLite)
//!   ├─ session_id (UUID)
//!   ├─ provider instance
//!   ├─ trust mode (plan/safe/auto)
//!   └─ cancellation token
//! ```
//!
//! This split allows the same agent to power multiple concurrent sessions
//! (e.g., main REPL + background sub-agents) without shared mutable state.

use crate::agent::KodaAgent;
use crate::bg_agent::{self, BgAgentRegistry};
use crate::config::KodaConfig;
use crate::db::Database;
use crate::engine::{EngineCommand, EngineSink};
use crate::file_tracker::FileTracker;
use crate::inference::InferenceContext;
use crate::providers::{self, ImageData, LlmProvider};
use crate::sub_agent_cache::SubAgentCache;
use crate::trust::TrustMode;

use anyhow::Result;
use koda_sandbox::{BuiltInProxy, BuiltInSocks5Proxy, DEFAULT_DEV_ALLOWLIST, Filter, ProxyHandle};
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Cached parse of [`DEFAULT_DEV_ALLOWLIST`].
///
/// **#1022 B22**: pre-fix, `Filter::new(DEFAULT_DEV_ALLOWLIST).expect(…)`
/// was called on every session creation. The args are static, the
/// result is identical, and parsing the regex set isn't free. More
/// importantly: a bad pattern in `DEFAULT_DEV_ALLOWLIST` would panic
/// at *every* session creation in production rather than failing
/// once at startup. The CI test
/// `koda_sandbox::filter::tests::default_allowlist_parses` already
/// guards the static, so the `expect` is sound — but hoisting into
/// a `OnceLock` makes the contract structural: parse once, panic
/// once if it ever does, clone-cheap (`Filter` is
/// `#[derive(Clone)]` and holds a `Vec<Pattern>`) for each session.
static DEV_ALLOWLIST_FILTER: OnceLock<Filter> = OnceLock::new();

/// Get (or initialize) the cached default-allowlist filter.
fn dev_allowlist_filter() -> &'static Filter {
    DEV_ALLOWLIST_FILTER
        .get_or_init(|| Filter::new(DEFAULT_DEV_ALLOWLIST).expect("default allowlist must parse"))
}

/// A single conversation session with its own state.
///
/// Each session has its own provider, trust mode, and cancel token.
/// Multiple sessions can share the same `Arc<KodaAgent>`.
pub struct KodaSession {
    /// Unique session identifier.
    pub id: String,
    /// Shared agent configuration (tools, system prompt).
    pub agent: Arc<KodaAgent>,
    /// Database handle for message persistence.
    pub db: Database,
    /// LLM provider for this session.
    pub provider: Box<dyn LlmProvider>,
    /// Current trust mode (Plan / Safe / Auto).
    pub mode: TrustMode,
    /// Cancellation token for graceful shutdown.
    pub cancel: CancellationToken,
    /// File lifecycle tracker — tracks files created by Koda (#465).
    pub file_tracker: FileTracker,
    /// Whether the session title has already been set (first-message guard).
    pub title_set: bool,
    /// Per-session HTTP CONNECT proxy (Phase 3b of #934).
    ///
    /// Spawned unconditionally in [`Self::new`] with the hardcoded
    /// [`koda_sandbox::DEFAULT_DEV_ALLOWLIST`] — koda is config-free,
    /// so there's no "opt in" toggle and no user-tunable allowlist
    /// (yet; future work: DB-backed slash command for per-project
    /// extensions). Always-on means every Bash invocation routes
    /// through this proxy and unknown hostnames get a 403 at the CONNECT
    /// layer.
    ///
    /// `Option` rather than bare [`ProxyHandle`] because spawn can fail
    /// (ephemeral-port exhaustion, broken loopback, runtime shutdown).
    /// Fail-open: on spawn failure we log + continue with `None`,
    /// matching the contract of [`koda_sandbox::ExternalProxy::spawn`].
    /// A broken proxy must never break a session — the kernel sandbox
    /// remains the authoritative network boundary anyway.
    ///
    /// Held for the session's lifetime; `Drop` aborts the proxy task
    /// and closes the listener — no manual teardown needed.
    pub proxy: Option<ProxyHandle>,

    /// Per-session SOCKS5 proxy (Phase 3d.1 of #934). Sibling of
    /// [`Self::proxy`] for raw-TCP clients (git over ssh, gRPC) that
    /// don't honor `HTTPS_PROXY`. Same fail-open contract: spawn
    /// failure logs a warning and the field stays `None`. Uses the
    /// same hostname allowlist as the HTTP proxy by construction —
    /// see [`koda_sandbox::BuiltInSocks5Proxy`].
    pub socks5_proxy: Option<ProxyHandle>,

    /// Background sub-agent registry (#1022 B12).
    ///
    /// Lives on the session, not on `inference_loop`, so background
    /// agents survive across turns. The previous design constructed
    /// the registry locally inside `inference_loop`; when the loop
    /// returned (final text, error, hard-stop) the `Arc` dropped and
    /// every still-pending bg task was aborted via
    /// [`tokio_util::task::AbortOnDropHandle`] — silently discarding
    /// any not-yet-completed result. With single-iteration responses
    /// (`InvokeAgent { background: true }` followed by final text in
    /// the same turn) this lost the bg result every time.
    ///
    /// Owning here means: bg tasks keep running between turns, and the
    /// next turn's first iteration drains anything that completed
    /// during the idle gap. Registry abort still happens at
    /// `Drop` — i.e. when the session itself is dropped — which is
    /// what users actually mean by "stop".
    ///
    /// Wrapped in `Arc` because tool dispatch needs to hand the same
    /// registry into the recursive `execute_sub_agent` call (so
    /// nested `InvokeAgent { background: true }` registers in the
    /// caller-visible slot, not a fresh per-call one).
    pub bg_agents: Arc<BgAgentRegistry>,

    /// Cross-turn sub-agent result cache (#1022 B12).
    ///
    /// Same lifetime motivation as [`Self::bg_agents`]: was previously
    /// re-created per `inference_loop` invocation, which threw away
    /// every cache entry on each turn boundary and made the cache
    /// useless for the natural "ask, follow up, ask again" flow.
    /// Living on the session means the second turn can hit results
    /// computed in the first.
    ///
    /// Invalidation still happens on every mutating tool call via
    /// `crate::tool_dispatch::execute_one_tool` — generation bump,
    /// cached entries with stale generations are treated as misses.
    /// Cross-turn doesn't change that contract; it just extends the
    /// window in which a still-fresh entry can be reused.
    pub sub_agent_cache: SubAgentCache,
}

impl KodaSession {
    /// Create a new session from an agent, config, and database.
    pub async fn new(
        id: String,
        agent: Arc<KodaAgent>,
        db: Database,
        config: &KodaConfig,
        mode: TrustMode,
    ) -> Self {
        let provider = providers::create_provider(config);
        // Wire db+session into ToolRegistry for RecallContext
        agent.tools.set_session(Arc::new(db.clone()), id.clone());

        // Start MCP servers from DB config (#662).
        //
        // Per-session ownership is intentional, not pending refactor (see #959).
        // Codex (closest peer agent) chose the same shape: per-session
        // `McpConnectionManager` in `SessionServices`, not app-level. App-level
        // ownership would complicate config-change semantics and lifecycle
        // management for an unmeasured startup-cost optimization. Reopen #959
        // if a real bug surfaces (e.g. multi-session resume becomes slow with
        // many configured servers).
        match crate::mcp::McpManager::start_from_db(&db).await {
            Ok(manager) => {
                if !manager.is_empty() {
                    let mgr = Arc::new(tokio::sync::RwLock::new(manager));
                    agent.tools.set_mcp_manager(mgr);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to start MCP servers (non-fatal)");
            }
        }
        let file_tracker = FileTracker::new(&id, db.clone()).await;

        // Spawn the per-session HTTP CONNECT proxy with the default dev
        // allowlist. Fail-open: on spawn failure, log + run unfiltered.
        // Always-on — koda is config-free, there's no "disable" knob.
        // **#1022 B22**: parse-once via `OnceLock` instead of
        // re-parsing the static allowlist on every session creation.
        // See `dev_allowlist_filter()` above for the rationale.
        let filter = dev_allowlist_filter().clone();
        let proxy = match BuiltInProxy::new(filter.clone()).spawn().await {
            Ok(handle) => {
                agent.tools.set_proxy_port(Some(handle.port));
                tracing::debug!(
                    "session {id} egress proxy listening on 127.0.0.1:{}",
                    handle.port
                );
                Some(handle)
            }
            Err(e) => {
                tracing::warn!(error = %e, "egress proxy spawn failed; running unfiltered");
                None
            }
        };

        // 3d.2: spin up the SOCKS5 sibling using the same allowlist.
        // Same fail-open contract as the HTTP proxy — raw-TCP clients
        // will fall through to whatever they'd do without ALL_PROXY
        // (i.e. dial direct, get caught by kernel-enforced egress where
        // present, or actually escape on platforms where it isn't).
        let socks5_proxy = match BuiltInSocks5Proxy::new(filter).spawn().await {
            Ok(handle) => {
                agent.tools.set_socks5_port(Some(handle.port));
                tracing::debug!(
                    "session {id} socks5 proxy listening on 127.0.0.1:{}",
                    handle.port
                );
                Some(handle)
            }
            Err(e) => {
                tracing::warn!(error = %e, "socks5 proxy spawn failed; raw-TCP clients unfiltered");
                None
            }
        };

        Self {
            id,
            agent,
            db,
            provider,
            mode,
            cancel: CancellationToken::new(),
            file_tracker,
            title_set: false,
            proxy,
            socks5_proxy,
            // #1022 B12: registry + cache live on the session so bg
            // agents survive across turns and the cache yields
            // cross-turn hits.
            bg_agents: bg_agent::new_shared(),
            sub_agent_cache: SubAgentCache::new(),
        }
    }

    /// Run one inference turn: prompt → streaming → tool execution → response.
    ///
    /// Emits `TurnStart` and `TurnEnd` lifecycle events. The loop-cap prompt is handled via `EngineEvent::LoopCapReached` / `EngineCommand::LoopDecision`
    /// through the `cmd_rx` channel.
    ///
    /// # Per-turn cancellation (#1208)
    ///
    /// `turn_cancel` lets callers (notably the TUI) wire Ctrl+C / Esc to a
    /// **per-turn** child token that, when fired, stops the inference loop
    /// without cancelling the session-lifetime `self.cancel` (which bg agents
    /// derive from — see #1200 for why session.cancel must stay stable across
    /// turns). When `None`, the inference loop falls back to `self.cancel`,
    /// which preserves the pre-#1208 behaviour every test, the headless
    /// driver, and the ACP server already rely on.
    pub async fn run_turn(
        &mut self,
        config: &KodaConfig,
        pending_images: Option<Vec<ImageData>>,
        sink: &dyn EngineSink,
        cmd_rx: &mut mpsc::Receiver<EngineCommand>,
        turn_cancel: Option<CancellationToken>,
    ) -> Result<()> {
        let turn_id = uuid::Uuid::new_v4().to_string();
        sink.emit(crate::engine::EngineEvent::TurnStart {
            turn_id: turn_id.clone(),
        });

        // Compose the per-turn system prompt: static `agent.system_prompt`
        // plus a dynamically-rendered MCP server-instructions section. We
        // do this per-turn (not at agent build time) because MCP servers
        // attach inside `KodaSession::new`, AFTER the static prompt is
        // built and the agent is wrapped in `Arc`. Composing here picks up
        // both the initial-connect case and any mid-session `/mcp add`
        // hot-reloads automatically (#922).
        let mcp_section = if let Some(mgr) = self.agent.tools.mcp_manager() {
            // Bind the Arc to extend its lifetime past the read guard
            // (try_read() returns a guard that borrows the lock).
            match mgr.try_read() {
                Ok(guard) => {
                    crate::prompt::render_mcp_instructions_section(&guard.server_instructions())
                }
                Err(_) => String::new(), // manager momentarily locked; skip this turn
            }
        } else {
            String::new()
        };
        let system_prompt = if mcp_section.is_empty() {
            self.agent.system_prompt.clone()
        } else {
            format!("{}{mcp_section}", self.agent.system_prompt)
        };

        let result = crate::inference::inference_loop(InferenceContext {
            project_root: &self.agent.project_root,
            config,
            db: &self.db,
            session_id: &self.id,
            system_prompt: &system_prompt,
            provider: self.provider.as_ref(),
            tools: &self.agent.tools,
            tool_defs: &self.agent.tool_defs,
            pending_images,
            mode: self.mode,
            sink,
            // #1208: prefer the caller-supplied per-turn cancel so the TUI
            // can stop *just this turn* with Ctrl+C without nuking the
            // session-lifetime token bg agents share. Headless / server /
            // tests pass `None` and keep the legacy session-token behaviour.
            cancel: turn_cancel.unwrap_or_else(|| self.cancel.clone()),
            cmd_rx,
            file_tracker: &mut self.file_tracker,
            bg_agents: &self.bg_agents,
            sub_agent_cache: &self.sub_agent_cache,
        })
        .await;

        let reason = match &result {
            Ok(()) if self.cancel.is_cancelled() => crate::engine::event::TurnEndReason::Cancelled,
            Ok(()) => crate::engine::event::TurnEndReason::Complete,
            Err(e) => crate::engine::event::TurnEndReason::Error {
                message: e.to_string(),
            },
        };
        sink.emit(crate::engine::EngineEvent::TurnEnd { turn_id, reason });

        result
    }

    /// Replace the provider (e.g., after switching models or providers).
    pub fn update_provider(&mut self, config: &KodaConfig) {
        self.provider = providers::create_provider(config);
    }
}

#[cfg(test)]
mod b22_tests {
    //! **#1022 B22** regression tests.
    //!
    //! These pin the OnceLock semantics: parse exactly once, return
    //! the same instance across calls, and stay valid across
    //! threads. Without these, a future "helpful" refactor that
    //! moves the cache to a thread-local or a `RwLock<Option<...>>`
    //! would silently re-introduce the per-session reparse cost
    //! (or, worse, the per-session panic path).
    use super::dev_allowlist_filter;
    use koda_sandbox::DEFAULT_DEV_ALLOWLIST;

    #[test]
    fn dev_allowlist_filter_is_singleton() {
        let a = dev_allowlist_filter();
        let b = dev_allowlist_filter();
        // Same `&'static` reference \u2014 not just equal contents.
        // OnceLock guarantees this; if someone refactors to a Box
        // and clones, this fails fast.
        assert!(
            std::ptr::eq(a, b),
            "dev_allowlist_filter must return the same instance across calls"
        );
    }

    #[test]
    fn dev_allowlist_filter_matches_static_size() {
        let f = dev_allowlist_filter();
        // Sanity: every pattern in the static parsed and made it
        // into the filter. If a future patch silently drops
        // patterns (e.g. a filter with a max-size cap), this catches it.
        assert_eq!(f.len(), DEFAULT_DEV_ALLOWLIST.len());
    }

    #[test]
    fn dev_allowlist_filter_is_send_sync() {
        // `OnceLock<Filter>` requires `Filter: Send + Sync` to give
        // out `&'static Filter` across threads. Pin it.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<koda_sandbox::Filter>();
    }
}
