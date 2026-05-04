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
use crate::child_agent::{self, ChildAgentRegistry};
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
use parking_lot::RwLock;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Cloneable, session-detached handle to the session-lifetime cancel root (#1216).
///
/// Holds an `Arc<RwLock<CancellationToken>>` alias of the same root
/// owned by [`KodaSession`] — so a clone can be passed across borrow
/// boundaries (notably the TUI's `&mut self.session` window held by a
/// pinned `run_turn` future) and still drive [`Self::interrupt`].
///
/// Two operations:
/// - [`Self::current`]: snapshot of the *current* root token. Use this
///   anywhere you'd previously called `session.cancel.clone()`.
/// - [`Self::interrupt`]: fire-and-swap, identical semantics to
///   [`KodaSession::interrupt`].
#[derive(Clone)]
pub struct SessionCancel {
    inner: Arc<RwLock<CancellationToken>>,
}

impl Default for SessionCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCancel {
    /// Construct a fresh, never-cancelled root.
    ///
    /// Used by [`KodaSession::new`] for production sessions and by
    /// integration tests that need to inject a known handle into a
    /// struct-literal `KodaSession` (so they can drive
    /// [`Self::interrupt`] from the test harness).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(CancellationToken::new())),
        }
    }

    /// Clone of the current root. See [`KodaSession::cancel_token`].
    pub fn current(&self) -> CancellationToken {
        self.inner.read().clone()
    }

    /// Fire current root + swap in a fresh one. See [`KodaSession::interrupt`].
    pub fn interrupt(&self) {
        let mut guard = self.inner.write();
        guard.cancel();
        *guard = CancellationToken::new();
    }
}

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
    /// Session-lifetime cancellation root (#1216).
    ///
    /// Wrapped in [`RwLock`] so an `Esc`/`Ctrl+C` Gemini-style cascade
    /// can fire the current token (collapsing every per-turn child and
    /// every bg-agent child) **and** atomically swap in a fresh root
    /// before the next turn starts. Without the swap, a tokio
    /// [`CancellationToken`] stays cancelled forever once fired —
    /// every subsequent `child_token()` would be born already-cancelled
    /// and the session would be permanently poisoned.
    ///
    /// Read via [`Self::cancel_token`] (cheap clone of current root).
    /// Fired via [`Self::interrupt`] (cascade + swap, single atomic op).
    /// Direct field access is intentionally private — every external
    /// site must go through one of those two doors so the swap
    /// invariant is impossible to violate.
    /// Session-lifetime cancellation root (#1216).
    ///
    /// Wraps an [`Arc<RwLock<CancellationToken>>`] so an `Esc`/`Ctrl+C`
    /// Gemini-style cascade can fire the current token (collapsing
    /// every per-turn child and every bg-agent child) **and** atomically
    /// swap in a fresh root before the next turn starts. Without the
    /// swap, a tokio [`CancellationToken`] stays cancelled forever once
    /// fired — every subsequent `child_token()` would be born
    /// already-cancelled and the session would be permanently poisoned.
    ///
    /// The `Arc` indirection lets [`Self::cancel_handle`] hand out a
    /// cloneable [`SessionCancel`] that survives outside the
    /// `&mut self` borrow window held by long-lived futures (the TUI
    /// keeps one across `run_turn` so Esc/Ctrl+C can still call
    /// [`SessionCancel::interrupt`] mid-turn).
    ///
    /// # Invariant (enforced by convention)
    ///
    /// Production code reads via [`Self::cancel_token`] and fires via
    /// [`Self::interrupt`] — never poke at the inner token directly.
    /// The field is `pub` only because integration tests in
    /// `koda-core/tests/` need to inject a known root via struct-
    /// literal construction; bypassing `interrupt()` in non-test code
    /// breaks the swap invariant and permanently poisons the session.
    pub cancel: SessionCancel,
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
    pub bg_agents: Arc<ChildAgentRegistry>,

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
            cancel: SessionCancel::new(),
            file_tracker,
            title_set: false,
            proxy,
            socks5_proxy,
            // #1022 B12: registry + cache live on the session so bg
            // agents survive across turns and the cache yields
            // cross-turn hits.
            bg_agents: child_agent::new_shared(),
            sub_agent_cache: SubAgentCache::new(),
        }
    }

    /// Run one inference turn: prompt → streaming → tool execution → response.
    ///
    /// Emits `TurnStart` and `TurnEnd` lifecycle events. The loop-cap prompt is handled via `EngineEvent::LoopCapReached` / `EngineCommand::LoopDecision`
    /// through the `cmd_rx` channel.
    ///
    /// Returns a clone of the **current** session-lifetime cancel token (#1216).
    ///
    /// Use this anywhere the legacy `self.cancel.clone()` was reached for.
    /// Hides the [`RwLock`] indirection from callers and keeps the
    /// swap-on-interrupt invariant invisible at call sites.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.current()
    }

    /// Cloneable handle to the cancel root for cross-borrow use (#1216).
    ///
    /// Returns a [`SessionCancel`] backed by the same `Arc` as the
    /// session's own field. The TUI clones this *before* the
    /// `run_turn` future borrows `&mut self.session`, so the
    /// Esc/Ctrl+C key handler can still call
    /// [`SessionCancel::interrupt`] mid-turn without the borrow
    /// checker getting in the way.
    pub fn cancel_handle(&self) -> SessionCancel {
        self.cancel.clone()
    }

    /// Cascade-cancel everything in this session and arm a fresh root (#1216).
    ///
    /// Fires the *current* root token — which propagates to every
    /// outstanding child: the live per-turn token (#1208), every bg
    /// agent's per-task token (#1200), nested bg agents, anything
    /// downstream that derived via [`CancellationToken::child_token`]
    /// off of [`Self::cancel_token`]. Then atomically swaps in a
    /// brand-new root so the *next* `run_turn` (and any bg agent it
    /// later spawns) starts from a clean, un-fired token.
    ///
    /// This is the Gemini-style cascade primitive: one call kills the
    /// whole tree, and the session remains usable for follow-up turns.
    /// See issue #1216 for the design discussion vs Codex (per-thread)
    /// and Claude Code (explicit two-tier) alternatives.
    pub fn interrupt(&self) {
        self.cancel.interrupt();
    }

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

        // #1216: snapshot the effective cancel token *before* handing
        // it to the inference loop. After the loop finishes we need to
        // know "did the cancellation that drove this turn happen?" —
        // but `self.cancel_token()` post-turn returns a *fresh* token
        // if [`Self::interrupt`] fired during the turn (the swap is
        // the whole point of #1216). Without snapshotting, every
        // interrupt-driven cancellation would be misreported as
        // `TurnEnd::Complete`. Caller-supplied per-turn tokens (#1208)
        // don't have this problem (they're not swapped) but we capture
        // uniformly for symmetry.
        let effective_cancel = turn_cancel.clone().unwrap_or_else(|| self.cancel_token());
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
            cancel: effective_cancel.clone(),
            cmd_rx,
            file_tracker: &mut self.file_tracker,
            bg_agents: &self.bg_agents,
            sub_agent_cache: &self.sub_agent_cache,
        })
        .await;

        let reason = match &result {
            Ok(()) if effective_cancel.is_cancelled() => {
                crate::engine::event::TurnEndReason::Cancelled
            }
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

#[cfg(test)]
mod cancel_handle_tests {
    //! #1216 — [`SessionCancel`] regression tests.
    //!
    //! These pin the Gemini-style cascade primitive's contract:
    //!
    //! 1. `interrupt()` fires the current root (cascade to children).
    //! 2. `interrupt()` swaps in a fresh root (subsequent `current()`
    //!    returns an UN-cancelled token — the session is reusable).
    //! 3. The cloneable handle aliases the same `Arc` so an `interrupt()`
    //!    on a clone is observed by every other clone (this is what
    //!    lets the TUI key handler interrupt across the `&mut self.session`
    //!    borrow held by the `run_turn` future).
    use super::SessionCancel;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_fires_current_root_cascading_to_children() {
        let h = SessionCancel::new();
        let child = h.current().child_token();
        let grandchild = child.child_token();
        assert!(!child.is_cancelled());
        assert!(!grandchild.is_cancelled());

        h.interrupt();
        // tokio::CancellationToken cascade is synchronous on `cancel()`,
        // so children observe immediately on the next poll.
        assert!(child.is_cancelled(), "child must observe cascade");
        assert!(grandchild.is_cancelled(), "grandchild must observe cascade");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_swaps_in_fresh_root_so_session_stays_usable() {
        let h = SessionCancel::new();
        let pre = h.current();
        h.interrupt();
        assert!(pre.is_cancelled(), "old root must be fired");
        let post = h.current();
        assert!(
            !post.is_cancelled(),
            "new root must be un-cancelled (else next turn is born dead)"
        );
        // And further children of the new root are independent of the old.
        let new_child = post.child_token();
        assert!(!new_child.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cloned_handles_share_underlying_arc() {
        let h1 = SessionCancel::new();
        let h2 = h1.clone();
        let child_via_h1 = h1.current().child_token();

        // Interrupt on the *clone* must cascade to children derived
        // from the original — this is the key invariant that lets
        // the TUI's detached handle interrupt mid-turn.
        h2.interrupt();
        assert!(
            child_via_h1.is_cancelled(),
            "clone's interrupt must fire the shared root"
        );

        // And after the swap, both handles see the same fresh root.
        let a = h1.current();
        let b = h2.current();
        assert!(!a.is_cancelled());
        assert!(!b.is_cancelled());
        // Firing through h1 cancels b's snapshot too (same root).
        h1.interrupt();
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_unblocks_a_waiter_within_a_few_ms() {
        // Exercises the same race that powers the WaitTask escape
        // hatch: a future awaiting `cancel.cancelled()` must wake up
        // promptly when interrupt fires from another task.
        let h = SessionCancel::new();
        let token = h.current();
        let h_for_fire = h.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            h_for_fire.interrupt();
        });
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(1), token.cancelled())
            .await
            .expect("must wake within 1s");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "must wake promptly, took {elapsed:?}"
        );
    }
}
