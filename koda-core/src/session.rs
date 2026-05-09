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
use crate::agent::inter_agent::InterAgentCommunication;
use crate::agent::mail_message::mail_to_user_message;
use crate::agent::mailbox::{Mailbox, MailboxReceiver};
use crate::agent::mailbox_registry::MailboxRegistry;
use crate::agent::path::AgentPath;
use crate::child_agent::{self, ChildAgentRegistry};
use crate::config::KodaConfig;
use crate::db::Database;
use crate::engine::{EngineCommand, EngineSink};
use crate::file_tracker::FileTracker;
use crate::inference::InferenceContext;
use crate::persistence::Persistence;
use crate::providers::{self, ImageData, LlmProvider};
use crate::sub_agent_cache::SubAgentCache;
use crate::trust::TrustMode;

use anyhow::Result;
use koda_sandbox::{BuiltInProxy, BuiltInSocks5Proxy, DEFAULT_DEV_ALLOWLIST, Filter, ProxyHandle};
use parking_lot::RwLock;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
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
    /// (an `InvokeAgent` dispatch followed by final text in the same
    /// turn — the common shape post-#1163 since every dispatch is
    /// now spawn-and-return) this lost the bg result every time.
    ///
    /// Owning here means: bg tasks keep running between turns, and the
    /// next turn's first iteration drains anything that completed
    /// during the idle gap. Registry abort still happens at
    /// `Drop` — i.e. when the session itself is dropped — which is
    /// what users actually mean by "stop".
    ///
    /// Wrapped in `Arc` because tool dispatch needs to hand the same
    /// registry into the recursive `execute_sub_agent` call (so
    /// nested `InvokeAgent` calls inside a spawned sub-agent register
    /// in the caller-visible slot, not a fresh per-call one).
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

    /// Long-lived task that drains [`Self::bg_agents`]'s status
    /// channel and forwards every event to the user-attached
    /// [`EngineSink`] in real time.
    ///
    /// **#1321**: replaces the per-iteration `drain_status_events()`
    /// poll in `inference_loop` (and the 200ms `with_status_pump`
    /// hotfix it briefly carried). Mirrors Codex's per-child
    /// `forward_events` task in `codex-rs/core/src/codex_delegate.rs`
    /// and Claude Code's `for await` over `runAgent()` async
    /// generator. Both push events through their natural channel as
    /// they happen — no shared queue, no pump.
    ///
    /// `Option` because the receiver hasn't been taken (and the task
    /// hasn't been spawned) until the client calls
    /// [`Self::attach_event_sink`]. Headless paths and tests that
    /// never attach a sink stay on the legacy synchronous
    /// [`ChildAgentRegistry::drain_status_events`] route.
    ///
    /// `AbortOnDropHandle` so dropping the session cleanly aborts
    /// the forwarder; if the session outlives the sink (the channel
    /// closes from the consumer side) the forwarder exits naturally
    /// when its `recv()` returns `None`.
    ///
    /// `pub` (not `pub(crate)`) for the same reason every sibling
    /// field is: integration tests in `koda-core/tests/` construct
    /// `KodaSession` via struct literal and need to populate every
    /// field. Default is `None` (no forwarder spawned).
    pub event_forwarder: Option<tokio_util::task::AbortOnDropHandle<()>>,

    // ── Phase 1 of #1325: peer-agent message-passing substrate. ──────────
    //
    // Each session owns a mailbox; mail sent here lands in the next
    // turn's user input via `drain_mail_to_db` (called from
    // `run_turn`). Phase 2 wires the substrate; Phase 3 exposes the
    // peer tools (`spawn_agent`/`send_message`/`wait_agent`) that
    // exercise it from the LLM side.
    /// Cloneable sender handle for this session's mailbox. Hand out
    /// clones to peer agents that need to send mail to this session.
    ///
    /// Stored as `Arc<Mailbox>` (not bare `Mailbox`) because
    /// `Mailbox` itself isn't `Clone` — the per-mailbox `AtomicU64`
    /// sequence counter would diverge across clones. The `Arc` is
    /// the substrate's documented sharing pattern; the
    /// `mailbox_registry` (Phase 3) hands out clones of this same
    /// `Arc` so all peer-tool callers send into the one true
    /// counter.
    pub mailbox: Arc<Mailbox>,

    /// Drain side of the mailbox. `tokio::sync::Mutex` because
    /// `drain_mail_to_db` is `async` (does DB writes) and codex's
    /// reference implementation uses the async mutex too — keeps the
    /// concurrency contract identical. Single-consumer by
    /// construction (`MailboxReceiver` is not `Clone`).
    ///
    /// Drained at the top of every `run_turn` before the inference
    /// loop reads the conversation. **Phase 2 deferral**: codex has a
    /// `MailboxDeliveryPhase` enum that lets late-arriving mail go to
    /// either the current turn or the next one depending on whether
    /// final-answer text has streamed yet. Koda always treats it as
    /// CurrentTurn (drain at start, no mid-turn worry). When koda
    /// adopts streaming-aware semantics, port the phase enum then.
    pub mailbox_rx: Arc<AsyncMutex<MailboxReceiver>>,

    /// Mail enqueued for the next turn while no turn is active.
    ///
    /// **Why a separate queue from `mailbox` itself**: the mailbox
    /// `mpsc` lets producers fire whenever; this queue is the
    /// drain-and-stash buffer that survives between turns so a single
    /// drain at `run_turn` start picks up everything (including mail
    /// that arrived between turns). Mirrors codex's
    /// `idle_pending_input` field with identical semantics.
    ///
    /// Phase 2 keeps it as the raw wire format (`InterAgentCommunication`)
    /// rather than pre-formatted strings so a future `MessagePhase`
    /// migration can re-serialize from source without information loss.
    pub idle_pending_input: Arc<AsyncMutex<Vec<InterAgentCommunication>>>,

    /// Phase 3 of #1325 — path → mailbox lookup, the substrate
    /// under the `send_message` and `wait_for_mail` peer tools.
    ///
    /// Pre-populated with `AgentPath::root() → self.mailbox` at
    /// construction so the LLM can mail itself (the only valid
    /// recipient until Phase 4's `spawn_agent` lands and starts
    /// registering child paths). Exposed to tools by handing the
    /// `Arc` to `agent.tools.set_mailbox_registry(...)` after
    /// construction.
    pub mailbox_registry: Arc<MailboxRegistry>,

    /// This session's identity in the agent spawn tree. The root
    /// user-facing session is always `/root`; spawned children
    /// (Phase 4 of #1325) inherit a `/root/<name>` path computed
    /// at spawn time.
    ///
    /// Threaded through the inference loop to `TurnContext` and
    /// `ToolExecCtx::caller_agent_path`, so peer tools
    /// (`SendMessage`, `WaitForMail`, the upcoming `SpawnAgent`)
    /// can stamp the right `author` on outgoing mail and look up
    /// the right inbox for blocking reads. Without this field,
    /// every spawned agent would falsely claim `author = /root`
    /// and corrupt the spawn-tree topology that mail attribution
    /// relies on.
    pub agent_path: AgentPath,
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

        // #1325 Phase 2: per-session mailbox pair. Producer (`Mailbox`)
        // is cloneable; consumer (`MailboxReceiver`) is not — enforced
        // by `mpsc` semantics. Constructed unconditionally because the
        // substrate has zero runtime cost when no mail is sent.
        let (mailbox, mailbox_rx) = Mailbox::new();
        let mailbox = Arc::new(mailbox);

        // #1325 Phase 3: per-session path → mailbox registry. Pre-
        // register `/root → self.mailbox` so peer tools can find this
        // session by canonical path. Future spawned children
        // (Phase 4) register their own `/root/<name>` entries here.
        let mailbox_registry = Arc::new(MailboxRegistry::new());
        // Double-register would be a programmer error here (we just
        // built an empty registry); panic if it happens because it
        // means the substrate's invariants are wrong.
        match mailbox_registry.register(AgentPath::root(), Arc::clone(&mailbox)) {
            crate::agent::mailbox_registry::RegisterOutcome::Inserted => {}
            crate::agent::mailbox_registry::RegisterOutcome::AlreadyRegistered => {
                unreachable!(
                    "freshly-constructed MailboxRegistry already had /root registered — \
                     MailboxRegistry::new() invariants are broken"
                );
            }
        }
        // Hand the registry to the tool layer so peer tools can
        // resolve paths. `set_*` setters use interior mutability —
        // see ToolRegistry docs for the rationale.
        agent
            .tools
            .set_mailbox_registry(Arc::clone(&mailbox_registry));

        // #1338 Issue #3: hand the bg-agent registry to the tool
        // layer so `WaitForMail`'s timeout payload can list which
        // sub-agents are still running. Constructed here (instead of
        // inline in the `Self { bg_agents: ... }` literal below) so we
        // can `Arc::clone` it into both places without touching the
        // shape of the struct constructor.
        let bg_agents = child_agent::new_shared();
        agent.tools.set_bg_agents(Arc::clone(&bg_agents));

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
            bg_agents,
            sub_agent_cache: SubAgentCache::new(),
            // No forwarder yet; clients (TUI / ACP) call
            // `attach_event_sink` to spawn it. Headless paths leave
            // it `None` and rely on the legacy synchronous drain.
            event_forwarder: None,
            // #1325 Phase 2: per-session mailbox. The substrate is
            // wired but no LLM-facing tool produces mail yet — Phase
            // 3 lands `spawn_agent`/`send_message`/`wait_agent`.
            // Constructing here means the substrate is uniformly
            // available to test code and future tools without a
            // capability-detection branch elsewhere.
            mailbox,
            mailbox_rx: Arc::new(AsyncMutex::new(mailbox_rx)),
            idle_pending_input: Arc::new(AsyncMutex::new(Vec::new())),
            mailbox_registry,
            // The user-facing root session is always `/root`. Phase 4's
            // spawn_agent will construct child sessions with
            // `/root/<name>` paths via a separate constructor (or this
            // one with an extra arg) — defer that change until the
            // spawn machinery lands.
            agent_path: AgentPath::root(),
        }
    }

    /// Wire the registry's status-event channel into a long-lived
    /// forwarder task that emits each event to `sink` the moment it
    /// lands.
    ///
    /// **Idempotent across the registry's lifetime**: the receiver
    /// is moved out of the registry exactly once via
    /// [`ChildAgentRegistry::take_event_receiver`], so calling this
    /// twice is a no-op (returns `false` on the second call without
    /// spawning anything). Production wires it once at TUI / ACP
    /// startup and forgets about it.
    ///
    /// Returns `true` if the forwarder was spawned, `false` if the
    /// registry's receiver had already been taken (a previous call,
    /// or a foreign owner) — callers can use the bool to log a
    /// warning if double-attach is suspicious for their context.
    ///
    /// **Lifetime**: the spawned task lives as long as `self` lives;
    /// the `AbortOnDropHandle` in [`Self::event_forwarder`] aborts
    /// it on session drop. If the consumer side of `sink` closes
    /// (TUI exits, ACP socket dies) the forwarder exits naturally
    /// when its `recv()` next yields — the channel side stays
    /// healthy and any future re-attach reuses it.
    ///
    /// See module docs on [`crate::child_agent::ChildAgentRegistry`]
    /// for the design rationale (mirrors Codex's `forward_events`).
    pub fn attach_event_sink(&mut self, sink: Arc<dyn crate::engine::EngineSink>) -> bool {
        let Some(mut rx) = self.bg_agents.take_event_receiver() else {
            tracing::debug!(
                target: "koda_core::diag::child_activity",
                stage = "attach_event_sink",
                "event_receiver already taken \u{2014} forwarder NOT spawned"
            );
            return false;
        };
        tracing::debug!(
            target: "koda_core::diag::child_activity",
            stage = "attach_event_sink",
            "forwarder task spawned"
        );
        let handle = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let crate::engine::EngineEvent::ChildAgentActivity { task_id, .. } = &event {
                    tracing::debug!(
                        target: "koda_core::diag::child_activity",
                        stage = "forwarder_task",
                        task_id = task_id,
                        "forwarding ChildAgentActivity to sink.emit"
                    );
                }
                sink.emit(event);
            }
            tracing::debug!(
                target: "koda_core::diag::child_activity",
                stage = "forwarder_task",
                "forwarder loop exited (channel closed)"
            );
        });
        self.event_forwarder = Some(tokio_util::task::AbortOnDropHandle::new(handle));
        true
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

    // ── #1325 Phase 2: peer-agent mailbox plumbing. ────────────────────

    /// Borrow the cloneable [`Mailbox`] sender. Hand `clone()`s out
    /// to peer agents that need to mail this session.
    ///
    /// Phase 2 keeps this as a bare accessor; Phase 3's
    /// `send_message` tool will wrap it with caller-spawner scoping
    /// (Model E from #996) so an agent can only mail siblings/children
    /// it spawned, not arbitrary peers.
    pub fn mailbox(&self) -> &Mailbox {
        &self.mailbox
    }

    /// Queue mail for the next turn while no turn is active.
    ///
    /// Mirrors codex's `queue_response_items_for_next_turn` but takes
    /// the raw [`InterAgentCommunication`] (preserving wire fields)
    /// rather than a pre-formatted message. Drained alongside the
    /// mailbox itself by [`Self::drain_mail_to_db`].
    pub async fn enqueue_for_next_turn(&self, communication: InterAgentCommunication) {
        self.idle_pending_input.lock().await.push(communication);
    }

    /// Drain mailbox + idle queue into the session's persisted message
    /// history as user-role messages. Called from [`Self::run_turn`]
    /// before the inference loop reads the conversation, so any mail
    /// in flight at turn-start lands in the next LLM call.
    ///
    /// Order: idle queue first (FIFO across the gap between turns),
    /// then mailbox (FIFO within this turn-start drain). Matches
    /// codex's `take_queued_response_items_for_next_turn` + mailbox
    /// drain order so cross-codebase reasoning stays portable.
    ///
    /// **Phase 2 deferral**: codex's `MailboxDeliveryPhase` lets
    /// late-arriving mail go to either the current turn or the next
    /// one depending on whether final-answer text has streamed yet.
    /// We always treat it as CurrentTurn (drain at start, no mid-turn
    /// worry). When koda adopts streaming-aware semantics, port the
    /// phase enum then.
    pub async fn drain_mail_to_db(&self) -> Result<()> {
        // Drain idle-queue first so any cross-turn FYI mail lands
        // ahead of mid-turn deliveries that might reference it.
        let idle: Vec<InterAgentCommunication> =
            std::mem::take(&mut *self.idle_pending_input.lock().await);
        let mailbox_items: Vec<InterAgentCommunication> = self.mailbox_rx.lock().await.drain();

        if idle.is_empty() && mailbox_items.is_empty() {
            return Ok(());
        }

        for mail in idle.into_iter().chain(mailbox_items) {
            let (role, content) = mail_to_user_message(&mail);
            self.db
                .insert_message(&self.id, &role, Some(&content), None, None, None)
                .await?;
        }
        Ok(())
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

        // #1325 Phase 2: drain any peer-agent mail (mailbox + idle
        // queue) into persisted user messages BEFORE the inference
        // loop reads the conversation. This is the codex-equivalent
        // of `take_queued_response_items_for_next_turn` +
        // `get_pending_input` (which fold mail into `pending_input`).
        // Koda persists the conversation, so mail-as-user-message
        // achieves the same "LLM sees mail at turn start" semantics
        // without an in-memory pending-input vector.
        //
        // No tool produces mail yet (Phase 3 lands the peer tools);
        // the drain is a no-op until then. Failure here is fatal for
        // the turn — a partially-drained mailbox would silently lose
        // mail, which is worse than a visible error.
        if let Err(e) = self.drain_mail_to_db().await {
            sink.emit(crate::engine::EngineEvent::TurnEnd {
                turn_id,
                reason: crate::engine::event::TurnEndReason::Error {
                    message: format!("failed to drain mailbox: {e}"),
                },
            });
            return Err(e);
        }

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
            // #1325 Phase 4: this session's identity in the spawn
            // tree (always `/root` for the user-facing session;
            // future spawned sessions will carry their assigned
            // child path). Threaded into TurnContext → ToolExecCtx
            // so peer tools can stamp the right `author` on mail.
            agent_path: &self.agent_path,
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
        // Same `&'static` reference \u{2014} not just equal contents.
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
