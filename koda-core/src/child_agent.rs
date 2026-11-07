//! Live registry of the sub-agents running inside the current turn.
//!
//! Sub-agents run synchronously (#1366): `InvokeAgent` blocks until the
//! sub-agent returns and hands its answer back as the tool result. While
//! one runs, `sub_agent_dispatch` registers it here so clients can show
//! what it is doing: the TUI's activity overlay reads
//! [`crate::child_agent::ChildAgentRegistry::snapshot`], and every status
//! change or tool call inside the sub-agent reaches the engine sink as an
//! `EngineEvent::ChildTaskUpdate` / `ChildAgentActivity` through the
//! [`crate::child_agent::ChildStatusEmitter`]. The entry is removed when
//! the sub-agent returns ([`crate::child_agent::FgRegistrationGuard`]).
//!
//! The background-spawn path this registry was built for (a `tokio::spawn`
//! per `InvokeAgent`, results drained and injected as user messages) was
//! unreachable after #1366 and has been deleted; the `is_background` flag
//! stays on snapshots and wire events, always `false`, so the wire format
//! is unchanged.
//!
//! ## Cancellation
//!
//! Each entry holds a `child_token()` of the turn's cancel token. The
//! sub-agent's loop watches that child, so cancelling the turn stops it,
//! and so does [`crate::child_agent::ChildAgentRegistry::cancel`] on one
//! entry (`/cancel agent:N`).
//!
//! ## Thread safety
//!
//! The registry is wrapped in `Arc` and shared between the inference loop,
//! the dispatch path and the client's event forwarder.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
// **#1022 B16**: was `std::sync::Mutex`. Switched to `parking_lot::Mutex`
// for three reasons:
//   1. **No poisoning** — if a thread panics while holding the lock,
//      subsequent calls don't get a `PoisonError`. The bg registry
//      is shared between the main inference loop and N spawned tasks;
//      a panic in one critical section bricking every subsequent
//      drain would be a particularly bad failure mode.
//   2. **Faster on contention** — no atomic check for poison flag,
//      no `Result` allocation. The contention is real: the TUI
//      snapshots the registry on every frame while a sub-agent runs.
//   3. **Cleaner API** — `.lock()` returns a guard directly, no
//      `.unwrap()` boilerplate at every call site.
// We deliberately keep this *sync* (not `tokio::sync::Mutex`) because
// the critical sections are short HashMap ops with no awaits inside.
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::engine::EngineEvent;

// ── Layer 0 of #996 ──────────────────────────────────────────────────────
//
// Status enum + watch-channel plumbing + per-task cancel + snapshot API.
// Pure infrastructure: no slash commands, no LLM tools, no UI changes.
// Layers 1+ (slash commands, tools, status-bar pill) consume this surface.
//
// Modeled on Codex's `tokio::sync::watch::Receiver<AgentStatus>` pattern
// (codex-rs/core/src/session/mod.rs). The bg-agent task drives the
// `watch::Sender`; the registry stores the matching `Receiver` and exposes
// snapshots to whoever asks (slash command, LLM tool, status-bar pill).

/// Lifecycle of a single background sub-agent task.
///
/// The bg-agent future drives transitions through `watch::Sender<AgentStatus>`.
/// Initial value is [`AgentStatus::Pending`]; the future flips to `Running`
/// when execution actually starts and to one of the terminal variants
/// (`Completed`, `Errored`, `Cancelled`) when it finishes.
///
/// `Running.iter` reflects the current inference iteration. Background
/// agents emit live updates via Layer 4 (#1058); `0` is the entry-point
/// placeholder before the first iteration fires. There is no upper bound
/// (#1110 removed the sub-agent cap) — the counter widens to `u32` so
/// long-running agents can't silently wrap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentStatus {
    /// Reserved but the spawned future hasn't started yet.
    Pending,
    /// Actively executing. `iter` is the current inference iteration
    /// (1..); `0` means "started, no iter info yet" (Layer 0 default).
    /// There is no upper bound — see #1110 / `LoopDetector` for termination.
    Running {
        /// Current inference iteration (1..). `0` is the entry-point
        /// placeholder emitted before the first iteration in
        /// `run_bg_agent`; background agents update this live
        /// (Layer 4, #1058). `u32` because #1110 removed the sub-agent
        /// iteration cap and a meandering weak model could plausibly
        /// exceed `u8::MAX` before context exhaustion.
        iter: u32,
    },
    /// User or parent fired the cancel token. Terminal.
    Cancelled,
    /// Sub-agent returned a final answer. Terminal.
    Completed {
        /// The agent's final output. Truncation for display is the
        /// renderer's job (see Codex's `COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES`).
        summary: String,
    },
    /// Sub-agent returned an error. Terminal.
    Errored {
        /// Error message as produced by `execute_sub_agent`. Same
        /// truncation note as `Completed.summary`.
        error: String,
    },
}

/// Snapshot of a pending bg-agent task — what `/agents` and the
/// `ListBackgroundTasks` LLM tool will render.
///
/// Cloned out of the registry under the lock so callers can format/display
/// without holding it. `age` is computed from `started_at` at snapshot time.
///
/// `#[non_exhaustive]`: external crates may add fields like `parent_task_id`,
/// `cancel_token`, or daemon-side metadata in future minor releases without
/// it being a breaking change. Constructed only inside `koda-core`, so the
/// attribute imposes zero friction on consumers (they only read fields).
///
/// `#[must_use]`: ignoring a snapshot is almost always a bug — the only
/// reason to call `snapshot()` is to inspect or render the contents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "a snapshot is only useful if you read its fields"]
pub struct ChildTaskSnapshot {
    /// Monotonic id assigned at registration. Stable for the
    /// lifetime of the task; reused across snapshots.
    pub task_id: u32,
    /// Configured agent name (`explore`, `task`, ...).
    pub agent_name: String,
    /// The prompt the parent delegated, verbatim; truncation is the
    /// renderer's job.
    pub prompt: String,
    /// Wall-clock duration since the task was registered. Computed at
    /// snapshot time, so successive snapshots of the same task
    /// report different ages.
    pub age: Duration,
    /// Latest value from the task's `watch::Receiver<AgentStatus>`.
    pub status: AgentStatus,
    /// Always `false`: every sub-agent runs inside the turn that
    /// called it. Kept so snapshots and the `ChildTaskUpdate` wire
    /// event keep their shape from the background-spawn era.
    pub is_background: bool,
    /// Sub-agent invocation that started this one. `None` = top-level
    /// (the user's main conversation).
    pub spawner: Option<u32>,
}

impl ChildTaskSnapshot {
    /// Test-only constructor for downstream crates (e.g. `koda-cli`
    /// renderer tests). The `#[non_exhaustive]` attribute on the
    /// struct forbids brace-init from outside this crate; this
    /// helper preserves that invariant for production code while
    /// still letting renderer-layer tests build fixtures without
    /// spinning up a full `ChildAgentRegistry` + `tokio::spawn`.
    ///
    /// Hidden from rustdoc (`#[doc(hidden)]`) so it doesn't pollute
    /// the public surface; intentional opt-out of API stability.
    #[doc(hidden)]
    pub fn for_testing(
        task_id: u32,
        agent_name: String,
        prompt: String,
        age: std::time::Duration,
        status: AgentStatus,
        spawner: Option<u32>,
    ) -> Self {
        Self {
            task_id,
            agent_name,
            prompt,
            age,
            status,
            // PR-A0.5 of #1232: legacy test fixtures predate the fg/bg
            // split, so default to `true` to match historical behavior
            // (every test agent today is a bg agent). Tests that need
            // to assert fg behavior should use `for_testing_fg`.
            is_background: true,
            spawner,
        }
    }

    /// Test-only constructor for foreground child-agent fixtures.
    /// Same shape as [`Self::for_testing`] but with `is_background`
    /// flipped to `false`. Added in PR-A0.5 of #1232 so PR-A's
    /// fg-routing tests can assert overlay behavior under the new
    /// `is_background == false` path without resurrecting the
    /// brace-init bypass that `#[non_exhaustive]` forbids.
    #[doc(hidden)]
    pub fn for_testing_fg(
        task_id: u32,
        agent_name: String,
        prompt: String,
        age: std::time::Duration,
        status: AgentStatus,
        spawner: Option<u32>,
    ) -> Self {
        Self {
            task_id,
            agent_name,
            prompt,
            age,
            status,
            is_background: false,
            spawner,
        }
    }
}

/// One running sub-agent.
struct ChildAgentEntry {
    agent_name: String,
    prompt: String,
    /// Per-task cancel, a `child_token()` of the turn's token. The
    /// sub-agent's loop watches it, so firing it (through
    /// [`ChildAgentRegistry::cancel`]) stops that sub-agent on its
    /// next loop iteration.
    cancel: CancellationToken,
    /// Live status channel — the dispatch path writes; the registry
    /// reads at snapshot time. See [`AgentStatus`] for the lifecycle.
    status_rx: watch::Receiver<AgentStatus>,
    /// When the sub-agent was registered. Used to compute `age` in snapshots.
    started_at: Instant,
    /// Sub-agent invocation that started this one. `None` = top-level.
    spawner: Option<u32>,
    /// Always `false` now; kept so snapshots and wire events keep their shape.
    is_background: bool,
}

/// Registry of the sub-agents running inside the current turn.
///
/// Shared via `Arc` between the inference loop, the dispatch path
/// (which registers each sub-agent for the duration of its run) and
/// the client's event forwarder.
pub struct ChildAgentRegistry {
    pending: Mutex<HashMap<u32, ChildAgentEntry>>,
    next_id: Mutex<u32>,
    /// Sender half of an unbounded mpsc that producers (every
    /// [`ChildStatusEmitter`] clone) call to deliver status events.
    ///
    /// **#1321 architecture**: pre-#1321 koda used a
    /// `Mutex<VecDeque<EngineEvent>>` polled once per parent
    /// inference iteration. That left bg-agent activity invisible for
    /// the entire duration of any long tool call (notably `WaitTask`,
    /// up to 300s) — events sat in the queue, then flushed in a
    /// stale flood when the wait returned.
    ///
    /// The mpsc shape mirrors Codex's `tx_event: Sender<Event>` (see
    /// `codex-rs/core/src/session/session.rs:17`) and Claude Code's
    /// `async function* runAgent` yield-based event flow: producers
    /// push, a single consumer pulls continuously, no shared queue
    /// to drain on a timer.
    ///
    /// **Unbounded** because bg-agent emissions are at human-readable
    /// rates (one per tool call, not per token), the consumer is
    /// always-on (the session-spawned forwarder), and back-pressuring
    /// a producer would block sub-agent inference loops mid-turn for
    /// reasons the model can't reason about.
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Receiver half. `Some` until [`Self::take_event_receiver`] hands
    /// it to the session forwarder, then `None` forever.
    ///
    /// Held in `Mutex<Option<_>>` so we can move it out exactly once.
    /// While the receiver still lives here (no forwarder attached —
    /// the headless / test path) callers can still drain events via
    /// [`Self::drain_status_events`] using `try_recv`. Once moved out,
    /// `drain_status_events` returns empty because the forwarder owns
    /// the rx and emits each event the moment it lands.
    event_rx: Mutex<Option<mpsc::UnboundedReceiver<EngineEvent>>>,
}

/// Fan-out helper for a sub-agent's status transitions.
///
/// Bundles the per-task `watch::Sender<AgentStatus>` (read by the
/// TUI overlay through the registry's snapshot API) with a
/// back-reference to the registry's event queue (forwarded to the
/// engine sink, so TUI and headless clients see the same events).
///
/// `Clone` is intentional: the dispatch path holds one copy for
/// status sends and wraps the sub-agent's sink with another for
/// activity events. Both share the same `watch::Sender` and
/// `Arc<registry>`, so every `.send()` reaches both fan-out targets.
#[derive(Clone)]
pub struct ChildStatusEmitter {
    task_id: u32,
    spawner: Option<u32>,
    is_background: bool,
    status_tx: watch::Sender<AgentStatus>,
    registry: Arc<ChildAgentRegistry>,
}

impl ChildStatusEmitter {
    /// Construct an emitter for one registry entry. Production code
    /// gets one from [`ChildAgentRegistry::register_fg_with_emitter`].
    pub fn new(
        task_id: u32,
        spawner: Option<u32>,
        is_background: bool,
        status_tx: watch::Sender<AgentStatus>,
        registry: Arc<ChildAgentRegistry>,
    ) -> Self {
        Self {
            task_id,
            spawner,
            is_background,
            status_tx,
            registry,
        }
    }

    /// Drive a status transition.
    ///
    /// Fans out to:
    /// 1. The per-task `watch::Sender` (so `snapshot()` / `/agents`
    ///    see the new state on the next read — no behavior change).
    /// 2. The registry's event queue, drained by the inference loop
    ///    and forwarded to the active `EngineSink` (so the TUI
    ///    / headless clients all see the same `ChildTaskUpdate` event).
    ///
    /// `watch::Sender::send` only fails if every receiver was dropped,
    /// which means the registry entry is gone — in that case the queue
    /// push is harmless (it'll be drained and ignored by clients that
    /// don't recognize the task id). We deliberately don't gate the
    /// queue push on the watch send result so a racing reap doesn't
    /// swallow the terminal `ChildTaskUpdate`.
    pub fn send(&self, status: AgentStatus) {
        let _ = self.status_tx.send(status.clone());
        self.registry
            .push_status_event(EngineEvent::ChildTaskUpdate {
                task_id: self.task_id,
                spawner: self.spawner,
                is_background: self.is_background,
                status,
            });
    }

    /// Forward a live activity event from inside the sub-agent up to
    /// the parent's sink.
    ///
    /// Paired with [`crate::engine::sink::FgForwardingSink`], which
    /// wraps the parent's sink for the duration of the sub-agent call
    /// and calls this method whenever an interesting child event
    /// happens (tool start/end, info line). The event lands on the
    /// same registry queue as `ChildTaskUpdate`, so it reaches
    /// whatever sink is active (TUI / headless) without further plumbing.
    ///
    /// Cheap: a single mutex acquisition + `VecDeque::push_back`,
    /// the same cost as [`Self::send`]. The activity feed runs at
    /// roughly tool-call frequency — not hot enough for any of this
    /// to register in a profile.
    pub fn send_activity(&self, kind: crate::engine::event::ChildAgentActivityKind) {
        self.registry
            .push_status_event(EngineEvent::ChildAgentActivity {
                task_id: self.task_id,
                spawner: self.spawner,
                // PR-A of #1232 §1: thread the emitter's stored
                // `is_background` flag through. Bg path passes `true`,
                // fg path passes `false`. Replaces PR-A0's hardcoded
                // `true` placeholder — the emitter has held the real
                // value since PR-A0.5.
                is_background: self.is_background,
                kind,
            });
    }

    /// Current status (read from the watch channel). Useful for
    /// terminal-disambiguation logic (e.g. "was this a cancel or a
    /// real error?") without taking the registry lock.
    pub fn current(&self) -> AgentStatus {
        self.status_tx.borrow().clone()
    }

    /// Test helper: peek the underlying watch sender. Production
    /// code should always go through [`Self::send`].
    #[cfg(test)]
    pub fn status_sender(&self) -> watch::Sender<AgentStatus> {
        self.status_tx.clone()
    }
}

/// RAII handle returned from [`ChildAgentRegistry::register_fg_with_emitter`].
///
/// The parent holds this guard for the duration of the sub-agent
/// call; on drop the registry entry is removed so the TUI overlay
/// stops showing the row. RAII covers every exit: a normal return,
/// a `?`-bubbled error and a panic. Without it an error path would
/// leak the entry, which the overlay would render as a phantom
/// "running" row.
///
/// The guard does *not* cancel the child token on drop: the sub-agent
/// has already returned by then.
pub struct FgRegistrationGuard {
    registry: Arc<ChildAgentRegistry>,
    task_id: u32,
    cancel: CancellationToken,
}

impl FgRegistrationGuard {
    /// The registry id assigned at registration time. Stable for the
    /// lifetime of the guard. Useful for tests asserting that the
    /// emitter and the guard refer to the same entry.
    pub fn task_id(&self) -> u32 {
        self.task_id
    }

    /// This sub-agent's cancel token: a `child_token()` of the token
    /// passed at registration, so it fires when the turn is cancelled
    /// and when [`ChildAgentRegistry::cancel`] names this entry. The
    /// dispatch path runs the sub-agent's loop on this token.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }
}

impl Drop for FgRegistrationGuard {
    fn drop(&mut self) {
        self.registry.pending.lock().remove(&self.task_id);
    }
}

impl ChildAgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
        }
    }

    /// Push an event onto the status channel. Called by
    /// [`ChildStatusEmitter::send`]; not part of the public API.
    ///
    /// Direct mpsc send — the consumer (either the session-spawned
    /// forwarder or, in headless / test paths, [`Self::drain_status_events`])
    /// observes the event the moment it lands. No queue, no pump,
    /// no per-iteration drain latency.
    ///
    /// **Send failure** (channel closed) is logged at `debug` and
    /// dropped on the floor. The only way to close the unbounded
    /// channel is to drop the registry itself, in which case the
    /// process is shutting down and no consumer cares about late
    /// events anyway.
    pub(crate) fn push_status_event(&self, event: EngineEvent) {
        if let Err(e) = self.event_tx.send(event) {
            tracing::debug!("ChildAgentRegistry status channel closed; dropping event: {e}");
        }
    }

    /// Move the receiver out of the registry so the session can spawn
    /// a long-lived forwarder task that drives the channel directly.
    ///
    /// Returns `Some(rx)` exactly once over the registry's lifetime;
    /// every subsequent call returns `None`. Mirrors Codex's
    /// `forward_events` pattern (`codex-rs/core/src/codex_delegate.rs`):
    /// one consumer per channel, owned by a dedicated task that runs
    /// for as long as the registry / session lives.
    ///
    /// **Headless / test paths** that never call this can keep using
    /// [`Self::drain_status_events`] — the receiver stays in the
    /// registry's `Mutex<Option<_>>` and `try_recv`-drain works as
    /// before. Production (the TUI) calls this exactly once at
    /// session startup so events flow continuously to the user's
    /// `EngineSink` regardless of what tool is currently in flight.
    pub fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<EngineEvent>> {
        self.event_rx.lock().take()
    }

    /// Drain queued status events synchronously.
    ///
    /// **Two modes**, transparent to the caller:
    ///
    /// 1. **Receiver still resident** (no forwarder attached yet —
    ///    headless, tests, the gap between session construction and
    ///    [`Self::take_event_receiver`]): pulls every queued event via
    ///    `try_recv` until the channel is empty, returns them in FIFO
    ///    order. Cheap: one mutex acquisition + a `try_recv` loop.
    ///
    /// 2. **Receiver moved to the forwarder**: returns an empty `Vec`
    ///    — the forwarder owns the rx and is emitting each event the
    ///    moment it lands; there is nothing left to drain here.
    ///
    /// Kept in the public API so existing callers (test suites, the
    /// pre-#1321 inference-loop drain we are about to delete) keep
    /// compiling without a rewrite. New code should rely on the
    /// session forwarder; only call this if you know the receiver
    /// hasn't been taken.
    pub fn drain_status_events(&self) -> Vec<EngineEvent> {
        let mut guard = self.event_rx.lock();
        let Some(rx) = guard.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Register a running sub-agent and hand back its live signal lines.
    ///
    /// Returns `(emitter, guard)`:
    /// * `emitter` — use `.send(...)` to push status transitions
    ///   (`Pending` → `Running { iter }` → terminal) and
    ///   `.send_activity(...)` to fan out tool/info events.
    /// * `guard` — a [`FgRegistrationGuard`] that removes the entry
    ///   when dropped and carries the sub-agent's cancel token, a
    ///   `child_token()` of `parent_cancel`. Hold it for the duration
    ///   of the sub-agent call and run the sub-agent's loop on
    ///   [`FgRegistrationGuard::cancel_token`].
    pub fn register_fg_with_emitter(
        self: &Arc<Self>,
        agent_name: &str,
        prompt: &str,
        parent_cancel: &CancellationToken,
        spawner: Option<u32>,
    ) -> (ChildStatusEmitter, FgRegistrationGuard) {
        let (status_tx, status_rx) = watch::channel(AgentStatus::Pending);
        let cancel = parent_cancel.child_token();
        let task_id = self.insert(agent_name, prompt, cancel.clone(), status_rx, spawner);
        let emitter = ChildStatusEmitter::new(task_id, spawner, false, status_tx, self.clone());
        let guard = FgRegistrationGuard {
            registry: self.clone(),
            task_id,
            cancel,
        };
        (emitter, guard)
    }

    fn insert(
        &self,
        agent_name: &str,
        prompt: &str,
        cancel: CancellationToken,
        status_rx: watch::Receiver<AgentStatus>,
        spawner: Option<u32>,
    ) -> u32 {
        let task_id = {
            let mut id = self.next_id.lock();
            let next = *id;
            *id += 1;
            next
        };
        self.pending.lock().insert(
            task_id,
            ChildAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                cancel,
                status_rx,
                started_at: Instant::now(),
                spawner,
                is_background: false,
            },
        );
        task_id
    }

    /// Test-only: register an entry and return its id.
    #[cfg(test)]
    pub fn register_test(&self, agent_name: &str, prompt: &str) -> u32 {
        self.register_test_with_status(agent_name, prompt, None).0
    }

    /// Test-only sibling of [`register_test`] that also returns the
    /// status sender, so a test can drive transitions by hand, and
    /// the entry's cancel token, so a test can watch it fire.
    #[cfg(test)]
    pub fn register_test_with_status(
        &self,
        agent_name: &str,
        prompt: &str,
        spawner: Option<u32>,
    ) -> (u32, watch::Sender<AgentStatus>, CancellationToken) {
        let (status_tx, status_rx) = watch::channel(AgentStatus::Pending);
        let cancel = CancellationToken::new();
        let task_id = self.insert(agent_name, prompt, cancel.clone(), status_rx, spawner);
        (task_id, status_tx, cancel)
    }

    // ── Layer 0 of #996: per-task cancel + snapshot ───────────────────────────────

    /// Fire the cancel token for a single task.
    ///
    /// Returns `true` if a pending task with that id existed and was
    /// signalled, `false` if the id is unknown (already drained,
    /// completed, or never registered). Idempotent: calling twice on
    /// the same id is safe — [`CancellationToken::cancel`] is itself
    /// idempotent.
    ///
    /// The entry stays in `pending` until the sub-agent's loop
    /// observes the token and returns, which drops its guard.
    pub fn cancel(&self, task_id: u32) -> bool {
        let guard = self.pending.lock();
        match guard.get(&task_id) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Snapshot every running sub-agent's metadata for the TUI overlay.
    ///
    /// `age` is computed against `Instant::now()` at call time, so two
    /// snapshots of the same task report different ages. Status is read
    /// from each entry's `watch::Receiver` (no blocking, no waiting).
    /// Sorted by ascending `task_id` so the output is stable across calls.
    pub fn snapshot(&self) -> Vec<ChildTaskSnapshot> {
        let guard = self.pending.lock();
        let now = Instant::now();
        let mut out: Vec<_> = guard
            .iter()
            .map(|(id, entry)| ChildTaskSnapshot {
                task_id: *id,
                agent_name: entry.agent_name.clone(),
                prompt: entry.prompt.clone(),
                age: now.saturating_duration_since(entry.started_at),
                status: entry.status_rx.borrow().clone(),
                is_background: entry.is_background,
                spawner: entry.spawner,
            })
            .collect();
        out.sort_by_key(|s| s.task_id);
        out
    }

    /// Clone the status receiver for a task so callers can await
    /// [`watch::Receiver::changed`] without holding the lock.
    ///
    /// Returns `None` if the entry is gone. Primarily used by tests to observe live iteration-counter updates
    /// without polling `snapshot()` in a tight loop.
    pub fn subscribe(&self, task_id: u32) -> Option<watch::Receiver<AgentStatus>> {
        let guard = self.pending.lock();
        guard.get(&task_id).map(|e| e.status_rx.clone())
    }
}

/// Outcome of a scoped cancel, used by
/// [`crate::tools::bg_process::BgRegistry::kill_as_caller`].
///
/// Mirrors HTTP-ish status codes so a caller can produce useful
/// error messages without inspecting registry internals.
///
/// Note: deliberately NOT `#[non_exhaustive]`. The `koda-cli` rendering
/// layer matches every variant exhaustively, and the compiler-driven
/// missed-variant warning we get from that is a feature, not a wart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "every CancelOutcome maps to a distinct response"]
pub enum CancelOutcome {
    /// Task existed, caller owned it, cancel token fired.
    Cancelled,
    /// No task with that id (never registered, or already gone).
    NotFound,
    /// Task exists but caller's `spawner` doesn't match.
    Forbidden,
}

impl Default for ChildAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Wrap in Arc for sharing between the inference loop, dispatch and the client.
pub fn new_shared() -> Arc<ChildAgentRegistry> {
    Arc::new(ChildAgentRegistry::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── Layer 0 of #996 ──────────────────────────────────────────────────────
    //
    // Status channel + per-task cancel + snapshot.

    /// `cancel(task_id)` must fire that task's cancel token: the hook
    /// `/cancel agent:N` calls. Verifies a known id returns true *and*
    /// the underlying token actually fires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_known_task_fires_token() {
        let reg = ChildAgentRegistry::new();
        let (task_id, _status_tx, observer) =
            reg.register_test_with_status("explore", "map repo", None);

        assert!(!observer.is_cancelled(), "precondition");
        let fired = reg.cancel(task_id);
        assert!(fired, "cancel(known_id) should report success");
        assert!(
            observer.is_cancelled(),
            "the task's cancel token should observe the cancellation"
        );
    }

    /// `cancel` on an unknown id must return false instead of
    /// panicking. The slash command surfaces this as "no such task".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_unknown_task_returns_false() {
        let reg = ChildAgentRegistry::new();
        assert!(
            !reg.cancel(999),
            "cancel of an unknown id should be a no-op returning false"
        );
    }

    /// `cancel` is idempotent — calling twice on the same id is safe
    /// (the underlying [`CancellationToken::cancel`] is itself
    /// idempotent). Both calls return true while the entry is still
    /// in `pending`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_is_idempotent_while_pending() {
        let reg = ChildAgentRegistry::new();
        let (task_id, _status_tx, _observer) = reg.register_test_with_status("explore", "x", None);

        assert!(reg.cancel(task_id));
        assert!(
            reg.cancel(task_id),
            "second cancel should still find the entry and report success"
        );
    }

    /// `snapshot()` must return one entry per pending task with
    /// stable ordering by `task_id`. Status defaults to `Pending`
    /// because no spawned future has flipped it yet.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_lists_pending_tasks_in_id_order() {
        let reg = ChildAgentRegistry::new();
        let id_a = reg.register_test("explore", "map");
        let id_b = reg.register_test("verify", "check");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        // Ordering is by ascending task_id, regardless of HashMap
        // iteration order — this is the contract the overlay relies on.
        assert_eq!(snap[0].task_id, id_a);
        assert_eq!(snap[0].agent_name, "explore");
        assert_eq!(snap[0].prompt, "map");
        assert_eq!(snap[0].status, AgentStatus::Pending);
        assert_eq!(snap[1].task_id, id_b);
        assert_eq!(snap[1].agent_name, "verify");
        assert_eq!(snap[1].status, AgentStatus::Pending);
    }

    /// `snapshot()` reads the live status channel — a `status_tx.send`
    /// must be observable on the very next snapshot, with no polling
    /// or yielding required (`watch::Receiver::borrow` is sync).
    /// This is the contract that lets the overlay reflect transitions
    /// immediately.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_reflects_status_writes() {
        let reg = ChildAgentRegistry::new();
        let (task_id, status_tx, _cancel) = reg.register_test_with_status("explore", "map", None);

        // Default is Pending.
        assert_eq!(reg.snapshot()[0].status, AgentStatus::Pending);

        // Flip to Running and observe.
        status_tx.send(AgentStatus::Running { iter: 3 }).unwrap();
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].task_id, task_id);
        assert_eq!(snap[0].status, AgentStatus::Running { iter: 3 });

        // Flip to Completed and observe.
        status_tx
            .send(AgentStatus::Completed {
                summary: "42 files".to_string(),
            })
            .unwrap();
        assert_eq!(
            reg.snapshot()[0].status,
            AgentStatus::Completed {
                summary: "42 files".to_string()
            }
        );
    }

    /// `snapshot()` reports a sane `age` that grows monotonically.
    /// We don't assert exact values (CI clocks are jittery) — just
    /// that two successive snapshots show a non-decreasing age and
    /// that the value is non-negative (saturating subtraction
    /// prevents underflow if the system clock jumps backwards).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_age_is_monotonic() {
        let reg = ChildAgentRegistry::new();
        let _id = reg.register_test("explore", "x");

        let age1 = reg.snapshot()[0].age;
        tokio::time::sleep(Duration::from_millis(15)).await;
        let age2 = reg.snapshot()[0].age;
        assert!(
            age2 >= age1,
            "age should be monotonic non-decreasing across snapshots"
        );
    }

    /// `snapshot()` on an empty registry returns an empty Vec, not a
    /// panic and not None.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_empty_registry_is_empty_vec() {
        let reg = ChildAgentRegistry::new();
        assert!(reg.snapshot().is_empty());
    }

    // ── #1076: ChildStatusEmitter → sink fan-out ───────────────────────────────

    /// Helper: build an emitter wired to the given registry, mirroring
    /// the production construction in `sub_agent_dispatch::execute_sub_agent`.
    fn emitter_for(
        reg: &Arc<ChildAgentRegistry>,
        task_id: u32,
        spawner: Option<u32>,
    ) -> ChildStatusEmitter {
        let (tx, _rx) = watch::channel(AgentStatus::Pending);
        ChildStatusEmitter::new(task_id, spawner, true, tx, reg.clone())
    }

    fn extract(event: &EngineEvent) -> (u32, Option<u32>, bool, &AgentStatus) {
        match event {
            EngineEvent::ChildTaskUpdate {
                task_id,
                spawner,
                is_background,
                status,
            } => (*task_id, *spawner, *is_background, status),
            other => panic!("expected ChildTaskUpdate, got {other:?}"),
        }
    }

    #[test]
    fn emitter_send_queues_engine_event_on_registry() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let emitter = emitter_for(&reg, 7, Some(42));

        // Initial: queue is empty.
        assert!(
            reg.drain_status_events().is_empty(),
            "fresh registry must have an empty event queue"
        );

        emitter.send(AgentStatus::Running { iter: 0 });
        let drained = reg.drain_status_events();
        assert_eq!(drained.len(), 1, "single send must produce one event");
        let (id, spawner, _is_bg, status) = extract(&drained[0]);
        assert_eq!(id, 7);
        assert_eq!(spawner, Some(42));
        assert!(matches!(status, AgentStatus::Running { iter: 0 }));
    }

    #[test]
    fn emitter_drain_is_fifo_and_clears_queue() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let emitter = emitter_for(&reg, 1, None);

        emitter.send(AgentStatus::Running { iter: 0 });
        emitter.send(AgentStatus::Running { iter: 1 });
        emitter.send(AgentStatus::Running { iter: 2 });
        emitter.send(AgentStatus::Completed {
            summary: "done".into(),
        });

        let drained = reg.drain_status_events();
        assert_eq!(drained.len(), 4, "all four sends must surface");

        // FIFO: transition order is preserved across batches.  This
        // matters for clients that render "iter N" progress — a
        // reorder would show the count moving backwards.
        let iters: Vec<_> = drained
            .iter()
            .filter_map(|e| match e {
                EngineEvent::ChildTaskUpdate {
                    status: AgentStatus::Running { iter },
                    ..
                } => Some(*iter),
                _ => None,
            })
            .collect();
        assert_eq!(iters, vec![0, 1, 2]);

        // Last event is the terminal Completed.
        assert!(matches!(
            extract(&drained[3]).3,
            AgentStatus::Completed { .. }
        ));

        // Drain consumes — second drain is empty.
        assert!(
            reg.drain_status_events().is_empty(),
            "drain must clear the queue"
        );
    }

    #[test]
    fn emitter_send_also_updates_watch_channel() {
        // The watch fan-out is what `/agents` and `snapshot()` read.
        // Sink fan-out (queue) is for the inference-loop → EngineSink
        // path.  Both targets must see every transition or `/agents`
        // and the TUI/headless clients will disagree on state.
        let reg = Arc::new(ChildAgentRegistry::new());
        let (tx, mut rx) = watch::channel(AgentStatus::Pending);
        let emitter = ChildStatusEmitter::new(3, None, true, tx, reg.clone());

        emitter.send(AgentStatus::Running { iter: 5 });

        // Watch channel observed.
        assert!(matches!(
            *rx.borrow_and_update(),
            AgentStatus::Running { iter: 5 }
        ));
        // Queue observed.
        let drained = reg.drain_status_events();
        assert_eq!(drained.len(), 1);
        assert!(matches!(
            extract(&drained[0]).3,
            AgentStatus::Running { iter: 5 }
        ));
    }

    #[test]
    fn emitter_clones_share_queue_and_watch() {
        // Layer 4 holds one clone for live `iter` heartbeats;
        // `run_bg_agent` keeps another for entry / terminal sends.
        // Both clones must funnel into the same registry queue and
        // the same per-task watch channel — otherwise terminal
        // states could land on a different queue than the heartbeats
        // and clients would see Running forever.
        let reg = Arc::new(ChildAgentRegistry::new());
        let (tx, _rx) = watch::channel(AgentStatus::Pending);
        let a = ChildStatusEmitter::new(11, Some(2), true, tx, reg.clone());
        let b = a.clone();

        a.send(AgentStatus::Running { iter: 1 });
        b.send(AgentStatus::Completed {
            summary: "ok".into(),
        });

        let drained = reg.drain_status_events();
        assert_eq!(drained.len(), 2, "clones must share the registry queue");
        // Watch channel reflects the LATEST send, regardless of which
        // clone made it (watch is overwriting by design).
        assert!(matches!(a.current(), AgentStatus::Completed { .. }));
    }

    #[test]
    fn agent_status_round_trips_through_serde() {
        // `EngineEvent::ChildTaskUpdate` is the wire format for
        // headless / future transports.  All `AgentStatus` variants
        // must survive a serde round-trip or the boundary leak fix
        // creates a new boundary leak (engine emits, transport drops
        // it on the floor).
        for status in [
            AgentStatus::Pending,
            AgentStatus::Running { iter: 0 },
            AgentStatus::Running { iter: 17 },
            AgentStatus::Cancelled,
            AgentStatus::Completed {
                summary: "hello".into(),
            },
            AgentStatus::Errored {
                error: "boom".into(),
            },
        ] {
            let event = EngineEvent::ChildTaskUpdate {
                task_id: 1,
                spawner: Some(2),
                is_background: true,
                status: status.clone(),
            };
            let json = serde_json::to_string(&event).expect("serialize");
            let back: EngineEvent = serde_json::from_str(&json).expect("deserialize");
            match back {
                EngineEvent::ChildTaskUpdate {
                    task_id,
                    spawner,
                    is_background,
                    status: round_tripped,
                } => {
                    assert_eq!(task_id, 1);
                    assert_eq!(spawner, Some(2));
                    assert!(is_background, "is_background must round-trip true");
                    assert_eq!(round_tripped, status, "json round-trip lost data: {json}");
                }
                other => panic!("round-trip changed variant: {other:?}"),
            }
        }
    }

    #[test]
    fn drain_status_events_is_empty_on_fresh_registry() {
        // The inference loop calls `drain_status_events` every
        // iteration; the no-bg-task case must be cheap and yield
        // an empty Vec without any allocations forced by mistakes
        // in the queue type (e.g. `Some(VecDeque::new())`).
        let reg = ChildAgentRegistry::new();
        let drained = reg.drain_status_events();
        assert!(drained.is_empty());
    }

    /// #1201 B: `ChildStatusEmitter::send_activity` must push events
    /// onto the *same* registry queue that `ChildStatusEmitter::send`
    /// uses, with the emitter's `task_id` and `spawner` baked in.
    /// This pins the wire-up so the inference loop's existing drain
    /// in `inference.rs` picks up activity events without any extra
    /// plumbing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_activity_queues_bg_child_activity_with_task_id() {
        let registry = new_shared();
        let (status_tx, _status_rx) = tokio::sync::watch::channel(AgentStatus::Pending);
        let emitter = ChildStatusEmitter::new(42, Some(7), true, status_tx, registry.clone());

        emitter.send_activity(crate::engine::event::ChildAgentActivityKind::ToolStart {
            tool_name: "Read".into(),
            summary: "Read foo.txt".into(),
        });
        emitter.send_activity(crate::engine::event::ChildAgentActivityKind::Info {
            message: "\u{26a1} cache hit".into(),
        });

        let drained = registry.drain_status_events();
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            &drained[0],
            EngineEvent::ChildAgentActivity {
                task_id: 42,
                spawner: Some(7),
                is_background: true,
                kind: crate::engine::event::ChildAgentActivityKind::ToolStart { tool_name, .. }
            } if tool_name == "Read"
        ));
        assert!(matches!(
            &drained[1],
            EngineEvent::ChildAgentActivity {
                task_id: 42,
                spawner: Some(7),
                is_background: true,
                kind: crate::engine::event::ChildAgentActivityKind::Info { message }
            } if message.contains("cache hit")
        ));
    }

    // ── PR-A of #1232 §1: foreground sub-agent registry path ────────────

    /// `register_fg_with_emitter` inserts an entry whose snapshot
    /// reports `is_background == false`. Sanity-checks that the fg
    /// path is wired to the same registry the `/agents` overlay reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_fg_inserts_entry_with_is_background_false() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let parent = CancellationToken::new();

        let (_emitter, _guard) =
            reg.register_fg_with_emitter("explore", "audit the storage layer", &parent, Some(7));

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1, "fg registration must add exactly one entry");
        let entry = &snap[0];
        assert_eq!(entry.agent_name, "explore");
        assert_eq!(entry.prompt, "audit the storage layer");
        assert_eq!(entry.spawner, Some(7));
        assert!(
            !entry.is_background,
            "fg entries must be tagged is_background=false so the overlay \
             and any future is_background-aware filter agree"
        );
    }

    /// Dropping the `FgRegistrationGuard` removes the entry.
    /// RAII covers `?`-bubbled errors and panics; without it the
    /// overlay would render a phantom "running" row indefinitely.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fg_guard_drop_removes_entry_from_registry() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let parent = CancellationToken::new();

        {
            let (_emitter, _guard) =
                reg.register_fg_with_emitter("explore", "do thing", &parent, None);
            assert_eq!(reg.snapshot().len(), 1, "entry present while guard alive");
        }

        assert_eq!(
            reg.snapshot().len(),
            0,
            "guard's Drop impl must remove the registry entry on scope exit"
        );
    }

    /// The guard's token is a child of the parent's: cancelling the
    /// turn stops the sub-agent, and so does `cancel(task_id)`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fg_cancel_token_observes_parent_and_per_task_cancel() {
        let reg = Arc::new(ChildAgentRegistry::new());

        let parent = CancellationToken::new();
        let (_e1, guard1) = reg.register_fg_with_emitter("explore", "a", &parent, None);
        assert!(!guard1.cancel_token().is_cancelled(), "precondition");
        parent.cancel();
        assert!(
            guard1.cancel_token().is_cancelled(),
            "the sub-agent's token must observe the parent cancel"
        );

        let parent = CancellationToken::new();
        let (_e2, guard2) = reg.register_fg_with_emitter("task", "b", &parent, None);
        assert!(reg.cancel(guard2.task_id()));
        assert!(
            guard2.cancel_token().is_cancelled(),
            "cancel(task_id) must fire the token the sub-agent's loop runs on"
        );
        assert!(
            !parent.is_cancelled(),
            "a per-task cancel must not cancel the turn"
        );
    }

    /// Fg emitter's `send_activity` must tag events with
    /// `is_background: false`. Pre-PR-A the emitter hardcoded `true`,
    /// which would have made every fg activity event mis-tagged on
    /// the wire — headless filters keying on that flag would
    /// silently drop fg events.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fg_emitter_tags_activity_events_with_is_background_false() {
        let reg = Arc::new(ChildAgentRegistry::new());
        let parent = CancellationToken::new();

        let (emitter, _guard) = reg.register_fg_with_emitter("explore", "prompt", &parent, None);
        emitter.send_activity(crate::engine::event::ChildAgentActivityKind::ToolStart {
            tool_name: "Read".into(),
            summary: "Read foo.rs".into(),
        });

        let drained = reg.drain_status_events();
        let activity = drained
            .iter()
            .find(|e| matches!(e, EngineEvent::ChildAgentActivity { .. }))
            .expect("activity event present");
        let EngineEvent::ChildAgentActivity { is_background, .. } = activity else {
            unreachable!("matched above");
        };
        assert!(
            !*is_background,
            "fg emitter must produce ChildAgentActivity {{ is_background: false }}; \
             pre-PR-A this was hardcoded true and would silently mis-tag the wire"
        );
    }

    // ─── #1321: mpsc-channel receiver semantics ──────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn take_event_receiver_returns_some_then_none() {
        // Contract: the receiver moves out of the registry exactly
        // once. Production calls this from `KodaSession::attach_event_sink`
        // at TUI startup; a second call must be a safe no-op (returns
        // `None`) so accidental double-attach doesn't deadlock or
        // panic. Mirrors Codex's `forward_events` lifecycle: spawn
        // once, abort once.
        let reg = ChildAgentRegistry::new();
        let first = reg.take_event_receiver();
        assert!(first.is_some(), "first call must yield the receiver");
        let second = reg.take_event_receiver();
        assert!(
            second.is_none(),
            "second call must return None — receiver is single-consumer"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_status_events_returns_empty_after_take() {
        // After the session forwarder takes the receiver, the legacy
        // synchronous drain path must be a benign no-op rather than
        // panicking or returning stale events. This guarantees the
        // headless-shaped helpers and the (now-removed) per-iteration
        // drain in `inference_loop` keep compiling for any external
        // caller that hasn't migrated yet.
        let reg = ChildAgentRegistry::new();
        // Push something *first*, then take the receiver: that event
        // belongs to the new owner, not to a late `drain_status_events`
        // caller.
        reg.push_status_event(EngineEvent::ChildTaskUpdate {
            task_id: 1,
            spawner: None,
            is_background: true,
            status: AgentStatus::Pending,
        });
        let mut rx = reg.take_event_receiver().expect("take must succeed");
        assert!(
            reg.drain_status_events().is_empty(),
            "drain after take must yield empty — events route through the receiver"
        );
        // The pre-take event reached the new receiver, not the void.
        let ev = rx
            .try_recv()
            .expect("pre-take event must reach the new owner");
        assert!(matches!(
            ev,
            EngineEvent::ChildTaskUpdate { task_id: 1, .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwarder_emits_during_blocking_consumer() {
        // **The actual bug fix from #1321.** Pre-#1321 a long-running
        // tool call (e.g. `WaitTask` blocking up to 300 s) starved the
        // bg-status drain because the drain only ran between
        // inference-loop iterations. With the mpsc receiver owned by a
        // dedicated forwarder task, status events reach the sink the
        // moment `push_status_event` fires — even while the would-be
        // drainer is parked inside an arbitrarily-long `await`.
        //
        // Simulates the failure shape: a fake "foreground" future that
        // sleeps for a long time, in parallel with bg-agent status
        // emissions. Asserts that each emission reaches the sink
        // *before* the foreground future returns.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        struct CountingSink(Arc<AtomicUsize>);
        impl crate::engine::EngineSink for CountingSink {
            fn emit(&self, _ev: EngineEvent) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let reg = Arc::new(ChildAgentRegistry::new());
        let count = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn crate::engine::EngineSink> = Arc::new(CountingSink(count.clone()));

        // Stand up the forwarder by hand (mirrors `attach_event_sink`).
        let mut rx = reg.take_event_receiver().unwrap();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                sink.emit(ev);
            }
        });

        // Foreground "tool call" that holds the loop hostage.
        let foreground = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(300)).await;
        });

        // Producer: emits status events at intervals while the
        // foreground task is still in its `await`. Pre-#1321 these
        // would queue silently — invisible until the foreground
        // returned. Post-#1321 they reach the sink immediately.
        let reg_p = reg.clone();
        let producer = tokio::spawn(async move {
            for iter in 1..=5u32 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                reg_p.push_status_event(EngineEvent::ChildTaskUpdate {
                    task_id: 7,
                    spawner: None,
                    is_background: true,
                    status: AgentStatus::Running { iter },
                });
            }
        });

        producer.await.unwrap();
        // Wait for the forwarder to drain in-flight events. Cap at a
        // value well below the foreground sleep (300 ms) so a
        // regression — events queueing instead of streaming — fails
        // here, not by accident from the foreground completing first.
        let deadline = std::time::Instant::now() + Duration::from_millis(150);
        while count.load(Ordering::SeqCst) < 5 && std::time::Instant::now() < deadline {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let observed_before_foreground = count.load(Ordering::SeqCst);
        foreground.await.unwrap();

        assert_eq!(
            observed_before_foreground, 5,
            "all 5 bg-status events must reach the sink BEFORE the foreground \
             tool call returns; pre-#1321 this was 0 (queued, not flowing)"
        );

        // Drop registry so the channel closes and the forwarder exits.
        drop(reg);
        let _ = forwarder.await;
    }
}
