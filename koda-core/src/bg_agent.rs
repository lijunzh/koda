//! Background sub-agent registry.
//!
//! Tracks sub-agents spawned with `background: true` in `InvokeAgent`.
//! The inference loop drains completed results and injects them as
//! user-role messages so the model sees them on the next iteration.
//!
//! ## Lifecycle
//!
//! 1. **Spawn**: `InvokeAgent { background: true }` creates a tokio task
//! 2. **Track**: the task handle + metadata are stored in `BgAgentRegistry`
//! 3. **Poll**: before each inference call, the loop calls `drain_completed()`
//! 4. **Inject**: completed results are appended as user messages
//! 5. **Cleanup**: on registry drop, all pending task handles are aborted —
//!    no orphan futures, no leaked worktrees. (Phase 1 of #1022, B3.)
//!
//! ## Cancellation cascade
//!
//! Bg-agent tasks receive a `CancellationToken` derived from the parent's
//! token via `child_token()` (wired in `crate::sub_agent_dispatch`). When
//! the parent is cancelled, every bg child sees it; when the registry
//! drops without cancellation, [`tokio_util::task::AbortOnDropHandle`]
//! still aborts the futures so we never leak. Both paths are covered.
//! (Phase 1 of #1022, B2+B3.)
//!
//! ## Thread safety
//!
//! The registry is wrapped in `Arc<Mutex<>>` and shared between the main
//! inference loop and the background task spawner.

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
//      no `Result` allocation. The contention is real: `drain_completed`
//      runs on every loop iteration.
//   3. **Cleaner API** — `.lock()` returns a guard directly, no
//      `.unwrap()` boilerplate at every call site.
// We deliberately keep this *sync* (not `tokio::sync::Mutex`) because
// the critical sections are short HashMap ops with no awaits inside.
use parking_lot::Mutex;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

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
/// `Running.iter` reflects the current inference iteration (1..=20).
/// Background agents emit live updates via Layer 4 (#1058); `0` is
/// the entry-point placeholder before the first iteration fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Reserved but the spawned future hasn't started yet.
    Pending,
    /// Actively executing. `iter` is the current inference iteration
    /// (1..=20); `0` means "started, no iter info yet" (Layer 0 default).
    Running {
        /// Current inference iteration (1..=20). `0` is the
        /// entry-point placeholder emitted before the first iteration
        /// in `run_bg_agent`; background agents update this live
        /// (Layer 4, #1058).
        iter: u8,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgTaskSnapshot {
    /// Monotonic id assigned at `reserve()` time. Stable for the
    /// lifetime of the task; reused across snapshots.
    pub task_id: u32,
    /// Configured agent name (`explore`, `verify`, ...).
    pub agent_name: String,
    /// The prompt the parent delegated. Surfaced verbatim by
    /// `/agents -v`; truncation is the renderer's job.
    pub prompt: String,
    /// Wall-clock duration since the task was attached. Computed at
    /// snapshot time, so successive snapshots of the same task
    /// report different ages.
    pub age: Duration,
    /// Latest value from the task's `watch::Receiver<AgentStatus>`.
    pub status: AgentStatus,
    /// Sub-agent task that spawned this bg-agent. `None` = top-level
    /// (the user's main conversation).
    ///
    /// **#996 Layer 2 / Model D**: tracked so that when a sub-agent
    /// exits, [`BgAgentRegistry::cancel_for_spawner`] can fire the
    /// cancel token on every bg-agent it left behind. Mirrors
    /// Claude Code's `agentId` field on `LocalShellTaskState`
    /// (`prevents 10-day fake-logs.sh zombies`).
    ///
    /// We don't use this for permission scoping — any caller with a
    /// `task_id` can manage any task. Spawner is *only* a cleanup
    /// hook (matching Claude Code's flat-permissions design).
    pub spawner: Option<u32>,
}

/// Payload sent over the bg-agent oneshot.
///
/// Pre-#1022-B9 this was just `String` (the model's final output).
/// Now also carries the trace lines collected by
/// [`crate::engine::sink::BufferingSink`] so the inference loop
/// can surface them to the user when injecting the result.
///
/// The `Result<BgPayload, BgPayload>` shape preserves the prior
/// success/failure discrimination: `Ok` means `execute_sub_agent`
/// returned text, `Err` means it returned an error (the trace is
/// useful in *both* cases — the bg agent may have done several
/// steps before erroring).
pub type BgPayload = (String, Vec<String>);

/// A completed background agent result.
#[derive(Debug)]
pub struct BgAgentResult {
    /// The agent name that produced this result.
    pub agent_name: String,
    /// The original prompt that was delegated.
    pub prompt: String,
    /// The agent's output (or error message).
    pub output: String,
    /// Whether the agent succeeded.
    pub success: bool,
    /// **#1022 B9**: narrative trace lines captured by
    /// [`crate::engine::sink::BufferingSink`] inside the bg agent.
    /// Pre-fix this was implicitly always empty (bg agents ran with
    /// `NullSink`). Now populated with one line per significant
    /// event (tool start, info, auto-rejected approval) so the user
    /// can see what the bg agent did at result-injection time.
    /// Empty for the cancelled / panicked case (`output` carries the
    /// failure detail in those paths).
    pub events: Vec<String>,
}

/// Handle returned when a background agent is spawned.
///
/// Holds the task's [`tokio_util::task::AbortOnDropHandle`] so the
/// future is aborted if the registry is dropped before the task
/// completes (B3 of #1022). Also holds the per-task
/// [`CancellationToken`] so future per-task cancel commands
/// (`/cancel <id>` — see #996) have a hook to fire.
struct BgAgentEntry {
    agent_name: String,
    prompt: String,
    rx: oneshot::Receiver<Result<BgPayload, BgPayload>>,
    /// Per-task cancel — derived as a `child_token()` of the parent
    /// session's token at spawn time. Firing this token (via
    /// [`BgAgentRegistry::cancel`] for #996, or via the registry-drop
    /// path) causes the in-flight bg agent to observe `is_cancelled()`
    /// on its next loop iteration.
    cancel: CancellationToken,
    /// Live status channel — the spawned future writes; the registry
    /// reads at snapshot time. See [`AgentStatus`] for the lifecycle.
    status_rx: watch::Receiver<AgentStatus>,
    /// When the task was attached. Used to compute `age` in snapshots.
    started_at: Instant,
    /// Sub-agent task that spawned this bg-agent. `None` = top-level.
    /// **#996 Layer 2 / Model D** — see [`BgTaskSnapshot::spawner`].
    spawner: Option<u32>,
    /// Aborts the spawned task on drop. The bg path uses
    /// `tokio::spawn` on the multi-thread runtime (#1022 B5):
    /// `execute_sub_agent` returns an explicitly `Send`-bounded
    /// future, so abort works promptly at any await point. The
    /// cancel-token cascade is still the primary stop signal
    /// (so the bg task can run any cleanup it owns).
    _handle: AbortOnDropHandle<()>,
}

/// Registry of running background sub-agents.
///
/// Shared via `Arc` between the inference loop (which drains results)
/// and the tool dispatch (which spawns agents).
pub struct BgAgentRegistry {
    pending: Mutex<HashMap<u32, BgAgentEntry>>,
    next_id: Mutex<u32>,
}

/// Reservation slot returned by [`BgAgentRegistry::reserve`].
///
/// The two-phase pattern (`reserve` → spawn → `attach`) lets the
/// dispatcher hand the oneshot sender into the spawned future
/// *before* the future exists, so the spawned closure can `move` it
/// without referencing the registry. The `cancel` token is a
/// `child_token()` of the parent's cancel — fires either when the
/// parent fires (cascade) or when this slot is individually cancelled
/// (future per-task `/cancel <id>` UX, #996).
pub struct BgAgentReservation {
    /// Monotonically-assigned task ID. Surfaces in user-facing
    /// messages (`Background agent 'foo' started (task 7)`) and
    /// keys the per-task `/cancel <id>` UX (#996).
    pub task_id: u32,
    /// Sender half of the result oneshot. Move into the spawned
    /// future so it can deliver `Ok(output)` / `Err(message)`.
    pub tx: oneshot::Sender<Result<BgPayload, BgPayload>>,
    /// Receiver half. Move back into the registry via [`BgAgentRegistry::attach`]
    /// so `drain_completed()` can poll it.
    pub rx: oneshot::Receiver<Result<BgPayload, BgPayload>>,
    /// Per-task cancel token. Cloned for the spawned future
    /// (`bg_cancel`) and re-stored on the registry entry
    /// (`entry_cancel`); both halves observe parent cancellation
    /// because this is a `child_token()` of the parent.
    pub cancel: CancellationToken,
    /// Status sender — move into the spawned future. The future is
    /// the sole writer; it transitions through
    /// [`AgentStatus::Pending`] → `Running` → terminal.
    pub status_tx: watch::Sender<AgentStatus>,
    /// Status receiver — hand back to the registry via [`BgAgentRegistry::attach`]
    /// so `snapshot()` and `/agents` can read the current state without
    /// touching the spawn site.
    pub status_rx: watch::Receiver<AgentStatus>,
    /// Sub-agent task id of the spawner, or `None` for the top-level
    /// loop. Carried verbatim to [`BgAgentRegistry::attach`] so the
    /// entry knows who spawned it (Model D cleanup-on-exit).
    pub spawner: Option<u32>,
}

impl BgAgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Reserve a task ID and produce a oneshot sender + child cancel
    /// token for the spawn site to consume. Call [`Self::attach`] with
    /// the resulting `JoinHandle` to complete registration.
    ///
    /// `spawner` records who is reserving this slot. `None` means
    /// the top-level inference loop; `Some(invocation_id)` means a
    /// sub-agent invocation. Used by [`Self::cancel_for_spawner`] to
    /// reap children when a sub-agent exits (Model E cleanup-on-exit)
    /// and by [`Self::snapshot_for_caller`] to scope LLM-tool views.
    ///
    /// The two-phase shape (`reserve` → spawn → `attach`) exists
    /// because `tokio::spawn` produces the `JoinHandle` *after* the
    /// future is built, but the future needs to own the `tx` to deliver
    /// its result. Reservation gives us `tx` early; attach binds the
    /// handle once it exists.
    pub fn reserve(
        &self,
        parent_cancel: &CancellationToken,
        spawner: Option<u32>,
    ) -> BgAgentReservation {
        let (tx, rx) = oneshot::channel();
        let (status_tx, status_rx) = watch::channel(AgentStatus::Pending);
        let mut id = self.next_id.lock();
        let task_id = *id;
        *id += 1;
        BgAgentReservation {
            task_id,
            tx,
            rx,
            cancel: parent_cancel.child_token(),
            status_tx,
            status_rx,
            spawner,
        }
    }

    /// Bind a spawned task's metadata to a previously [`reserve`]d slot.
    ///
    /// `rx` must be the receiver paired with the `tx` handed out by
    /// `reserve`. Holding `handle` as `AbortOnDropHandle` ensures the
    /// task is aborted on registry drop (B3 of #1022). `status_rx`
    /// is the read half of the watch channel whose write half
    /// (`status_tx`) was moved into the spawned future.
    ///
    /// [`reserve`]: Self::reserve
    //
    // 9 args trips `clippy::too_many_arguments` (limit 7). Each one
    // is load-bearing: id + name + prompt are display metadata;
    // rx/cancel/status_rx are the three channels we own; spawner is
    // the cleanup-routing key (Model E); handle is the
    // AbortOnDropHandle. Bundling into a struct just to satisfy
    // a heuristic would add a one-use type for no readability win
    // — "practicality beats purity".
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        &self,
        reservation_id: u32,
        agent_name: &str,
        prompt: &str,
        rx: oneshot::Receiver<Result<BgPayload, BgPayload>>,
        cancel: CancellationToken,
        status_rx: watch::Receiver<AgentStatus>,
        spawner: Option<u32>,
        handle: tokio::task::JoinHandle<()>,
    ) {
        self.pending.lock().insert(
            reservation_id,
            BgAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                rx,
                cancel,
                status_rx,
                started_at: Instant::now(),
                spawner,
                _handle: AbortOnDropHandle::new(handle),
            },
        );
    }

    /// Convenience for tests: register a synthetic entry without a
    /// real spawned task. The provided `tx` can be used to fire the
    /// result manually. The handle is a noop spawned task that
    /// returns immediately, so `_handle` has something to abort.
    #[cfg(test)]
    pub fn register_test(
        &self,
        agent_name: &str,
        prompt: &str,
    ) -> (u32, oneshot::Sender<Result<BgPayload, BgPayload>>) {
        let (id, tx, _status_tx, _cancel) =
            self.register_test_with_status(agent_name, prompt, None);
        (id, tx)
    }

    /// Test-only sibling of [`register_test`] that returns the status
    /// sender so a test can manually drive transitions without
    /// needing a real spawned `run_bg_agent`. The cancel token also
    /// comes back so cancel-cascade tests can verify the channel.
    ///
    /// `spawner` is recorded on the entry so scope-filtering /
    /// kill-on-exit tests can exercise both the top-level (`None`)
    /// and sub-agent (`Some(id)`) paths.
    #[cfg(test)]
    pub fn register_test_with_status(
        &self,
        agent_name: &str,
        prompt: &str,
        spawner: Option<u32>,
    ) -> (
        u32,
        oneshot::Sender<Result<BgPayload, BgPayload>>,
        watch::Sender<AgentStatus>,
        CancellationToken,
    ) {
        let (tx, rx) = oneshot::channel();
        let (status_tx, status_rx) = watch::channel(AgentStatus::Pending);
        let mut id = self.next_id.lock();
        let task_id = *id;
        *id += 1;
        drop(id);
        let cancel = CancellationToken::new();
        let cancel_observer = cancel.clone();
        let noop = tokio::spawn(async {});
        self.pending.lock().insert(
            task_id,
            BgAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                rx,
                cancel,
                status_rx,
                started_at: Instant::now(),
                spawner,
                _handle: AbortOnDropHandle::new(noop),
            },
        );
        (task_id, tx, status_tx, cancel_observer)
    }

    /// Drain all completed background agents. Non-blocking — only takes
    /// entries whose oneshot has already resolved.
    pub fn drain_completed(&self) -> Vec<BgAgentResult> {
        let mut guard = self.pending.lock();
        let mut completed = Vec::new();
        let mut done_ids = Vec::new();

        for (id, entry) in guard.iter_mut() {
            match entry.rx.try_recv() {
                Ok(Ok((output, events))) => {
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output,
                        success: true,
                        events,
                    });
                }
                Ok(Err((err, events))) => {
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output: err,
                        success: false,
                        events,
                    });
                }
                Err(oneshot::error::TryRecvError::Empty) => {
                    // Still running
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending — task panicked or was cancelled.
                    // No events available (the buffering sink died with the task).
                    done_ids.push(*id);
                    completed.push(BgAgentResult {
                        agent_name: entry.agent_name.clone(),
                        prompt: entry.prompt.clone(),
                        output: "[background agent task was cancelled]".to_string(),
                        success: false,
                        events: Vec::new(),
                    });
                }
            }
        }

        for id in done_ids {
            guard.remove(&id);
        }

        completed
    }

    /// How many background agents are still running.
    pub fn pending_count(&self) -> usize {
        self.pending.lock().len()
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
    /// The entry stays in `pending` until the spawned future actually
    /// observes the token and finishes. `drain_completed()` then
    /// reaps it via the closed-sender path (or the future's terminal
    /// `tx.send` if it noticed and shut down cleanly).
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

    /// Snapshot every pending task's metadata for `/agents` and the
    /// `ListBackgroundTasks` LLM tool.
    ///
    /// `age` is computed against `Instant::now()` at call time, so two
    /// snapshots of the same task report different ages. Status is read
    /// from each entry's `watch::Receiver` (no blocking, no waiting).
    /// Sorted by ascending `task_id` so the output is stable across calls.
    ///
    /// **Unscoped**: returns every task regardless of spawner. Used by
    /// the TUI `/agents` command (humans want the global view) and as
    /// the engine of [`Self::snapshot_for_caller`] (which filters).
    pub fn snapshot(&self) -> Vec<BgTaskSnapshot> {
        let guard = self.pending.lock();
        let now = Instant::now();
        let mut out: Vec<_> = guard
            .iter()
            .map(|(id, entry)| BgTaskSnapshot {
                task_id: *id,
                agent_name: entry.agent_name.clone(),
                prompt: entry.prompt.clone(),
                age: now.saturating_duration_since(entry.started_at),
                status: entry.status_rx.borrow().clone(),
                spawner: entry.spawner,
            })
            .collect();
        out.sort_by_key(|s| s.task_id);
        out
    }

    /// Scoped snapshot for the `ListBackgroundTasks` LLM tool.
    ///
    /// **Model E scoping**: a caller only sees tasks whose `spawner`
    /// matches its own `caller_spawner`. Top-level callers pass `None`
    /// and see only top-level-spawned tasks; sub-agent callers pass
    /// `Some(invocation_id)` and see only their own.
    ///
    /// Strict equality — a sub-agent does NOT see sibling sub-agents'
    /// tasks, and the top-level does NOT see sub-agents' tasks via the
    /// LLM (the TUI's `/agents` command remains the global view).
    pub fn snapshot_for_caller(&self, caller_spawner: Option<u32>) -> Vec<BgTaskSnapshot> {
        self.snapshot()
            .into_iter()
            .filter(|s| s.spawner == caller_spawner)
            .collect()
    }

    // ── Layer 2 of #996 ───────────────────────────────────────────────
    //
    // Scoped cancel + cleanup-on-exit + WaitTask machinery.
    // The unscoped [`Self::cancel`] above stays for the TUI (humans get
    // the global view); LLM tools route through [`Self::cancel_as_caller`]
    // so a sub-agent can't reach across into a sibling's task.

    /// Scoped per-task cancel for the `CancelTask` LLM tool.
    ///
    /// **Model E permission rule** — `caller_spawner` must equal the
    /// task's `spawner`, with `None == None` (top-level can cancel
    /// top-level tasks; sub-agent invocation N can cancel only its
    /// own tasks). Returns [`CancelOutcome::Forbidden`] otherwise.
    ///
    /// The unscoped [`Self::cancel`] is the TUI's contract — humans
    /// at the keyboard implicitly have full authority. This method is
    /// the LLM's contract.
    pub fn cancel_as_caller(&self, task_id: u32, caller_spawner: Option<u32>) -> CancelOutcome {
        let guard = self.pending.lock();
        match guard.get(&task_id) {
            None => CancelOutcome::NotFound,
            Some(entry) if entry.spawner != caller_spawner => CancelOutcome::Forbidden,
            Some(entry) => {
                entry.cancel.cancel();
                CancelOutcome::Cancelled
            }
        }
    }

    /// Fire the cancel token on every task whose `spawner` matches.
    ///
    /// Called from the sub-agent dispatch path when an invocation
    /// exits (Model E cleanup-on-exit). Returns the number of tasks
    /// signalled, purely for tracing — the actual reaping happens
    /// later via [`Self::drain_completed`] once the futures observe
    /// their cancel tokens and finish.
    ///
    /// Idempotent: re-calling on the same `spawner` after all tasks
    /// have been reaped returns `0`.
    pub fn cancel_for_spawner(&self, spawner: u32) -> usize {
        let guard = self.pending.lock();
        let mut count = 0;
        for entry in guard.values() {
            if entry.spawner == Some(spawner) {
                entry.cancel.cancel();
                count += 1;
            }
        }
        count
    }
}

/// Outcome of [`BgAgentRegistry::cancel_as_caller`].
///
/// Mirrors HTTP-ish status codes so the LLM-tool layer can produce
/// useful error messages without inspecting registry internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Task existed, caller owned it, cancel token fired.
    Cancelled,
    /// No task with that id (already drained, never registered, or
    /// completed and reaped).
    NotFound,
    /// Task exists but caller's `spawner` doesn't match. The LLM
    /// surface translates this into a permission-style error.
    Forbidden,
}

/// Outcome of [`BgAgentRegistry::wait_for_completion`].
///
/// Encodes the four resolutions of a `WaitTask` call. The LLM-tool
/// layer translates each into a serialised payload the model receives.
#[derive(Debug)]
pub enum WaitOutcome {
    /// Task reached a terminal `Completed`/`Errored` status before
    /// the timeout fired. Carries the drained [`BgAgentResult`] so
    /// the same payload `drain_completed()` would have produced is
    /// surfaced directly to the caller. (Drain semantics: a task
    /// consumed via `wait_for_completion` is removed from `pending`
    /// so the next `drain_completed()` won't double-inject it.)
    Completed(BgAgentResult),
    /// Task was cancelled (parent token fired, peer cancelled, or
    /// the spawned future panicked). The task has been reaped.
    Cancelled,
    /// Timeout fired before the task reached a terminal state. The
    /// task is still in `pending` and may still complete on its own.
    /// Carries the most-recent status snapshot so the model can
    /// decide whether to wait again or move on.
    TimedOut(BgTaskSnapshot),
    /// No task with that id, or caller's spawner doesn't match.
    /// Same `Forbidden`/`NotFound` distinction as [`CancelOutcome`].
    NotFound,
    /// Caller doesn't own this task.
    Forbidden,
}

impl BgAgentRegistry {
    /// Block until a single task reaches a terminal state, or until
    /// `timeout` elapses. The tool layer is the sole caller; humans
    /// use `/cancel` (synchronous) and the auto-drain path.
    ///
    /// **Drain semantics**: on `Completed`/`Cancelled`, the task is
    /// removed from `pending` here so `drain_completed()` won't
    /// surface it again on the next inference iteration. (This is
    /// the resolution to the result-routing race we settled in
    /// design Decision 3 — `WaitTask` consumes; auto-drain becomes a
    /// no-op for that id.)
    ///
    /// **Scoping**: same Model E rule as [`Self::cancel_as_caller`].
    /// `caller_spawner` must equal the task's `spawner` exactly.
    ///
    /// **Timeout**: bounded by the caller. The tool layer caps this
    /// at 300 s before reaching here; we trust the bound but will
    /// happily wait whatever value is passed (handy for tests with
    /// `Duration::from_millis(50)`).
    pub async fn wait_for_completion(
        &self,
        task_id: u32,
        caller_spawner: Option<u32>,
        timeout: Duration,
    ) -> WaitOutcome {
        // Phase 1 — ownership check + grab a status receiver to await on.
        // We do NOT remove the entry yet: if we time out, it must remain
        // visible to the next `drain_completed()` and to other callers.
        let status_rx = {
            let guard = self.pending.lock();
            match guard.get(&task_id) {
                None => return WaitOutcome::NotFound,
                Some(entry) if entry.spawner != caller_spawner => {
                    return WaitOutcome::Forbidden;
                }
                Some(entry) => entry.status_rx.clone(),
            }
        };

        // Phase 2 — wait for terminal status or timeout.
        // We watch the status channel rather than the oneshot so we
        // can distinguish Cancelled (terminal, no payload) from
        // Completed (terminal, payload pending on the oneshot). The
        // spawned future writes status BEFORE sending the oneshot,
        // so by the time we observe a terminal status the oneshot is
        // either ready or about to be (sub-microsecond skew).
        let wait_fut = wait_for_terminal_status(status_rx);
        let result = tokio::time::timeout(timeout, wait_fut).await;

        match result {
            Err(_elapsed) => {
                // Timeout: the task is still pending. Re-snapshot so
                // the caller sees the latest status (it may have
                // transitioned `Pending` → `Running` while we waited).
                let snap = self.snapshot().into_iter().find(|s| s.task_id == task_id);
                match snap {
                    Some(s) => WaitOutcome::TimedOut(s),
                    // Task vanished mid-wait — drain reaped it. The
                    // result already injected into the conversation;
                    // tell the caller it's gone.
                    None => WaitOutcome::NotFound,
                }
            }
            Ok(()) => {
                // Terminal status observed. Pull the entry out so the
                // auto-drain path won't see it again, then await the
                // oneshot for the payload.
                //
                // PR #1043 review fix: previously this used
                // `try_recv` + a back-to-back retry. The retry was a
                // placebo — both calls run in the same scheduler tick
                // with no `await` between them, so when the bg future
                // sets `status_tx` *before* `tx.send` (the standard
                // ordering in `sub_agent_dispatch::run_bg_agent`),
                // a multi-thread runtime can wake the waiter on a
                // *different* worker, observe `Empty` twice, and
                // falsely report `Cancelled` for a successful task.
                //
                // `oneshot::Receiver` IS a `Future` — just `.await`
                // it. The bound is set by `wait_for_terminal_status`
                // having already observed the terminal status, so the
                // sender is either landing or already dropped (the
                // future writes status before sending the result, and
                // panics drop both). A short inner timeout caps the
                // "in-flight" window; the outer timeout is already
                // exhausted at this point.
                let entry = {
                    let mut guard = self.pending.lock();
                    let Some(entry) = guard.remove(&task_id) else {
                        // Drain raced us and reaped first. Rare but
                        // possible. The model already saw the result
                        // in the prior turn's auto-drain.
                        return WaitOutcome::NotFound;
                    };
                    entry
                    // `guard` drops here — explicit scope guarantees
                    // we don't hold the (non-Send) `parking_lot`
                    // guard across the upcoming `.await`.
                };
                let agent_name = entry.agent_name;
                let prompt = entry.prompt;
                match tokio::time::timeout(Duration::from_millis(50), entry.rx).await {
                    Ok(Ok(Ok((output, events)))) => WaitOutcome::Completed(BgAgentResult {
                        agent_name,
                        prompt,
                        output,
                        success: true,
                        events,
                    }),
                    Ok(Ok(Err((err, events)))) => WaitOutcome::Completed(BgAgentResult {
                        agent_name,
                        prompt,
                        output: err,
                        success: false,
                        events,
                    }),
                    // Sender dropped (panic) or 50ms elapsed without
                    // a value landing — surface as Cancelled. Both
                    // are degenerate cases: status said terminal,
                    // payload never arrived.
                    Ok(Err(_)) | Err(_) => WaitOutcome::Cancelled,
                }
            }
        }
    }
}

/// Wait for a `watch::Receiver<AgentStatus>` to report a terminal
/// variant (`Completed`, `Errored`, `Cancelled`).
///
/// Returns when the current value is already terminal OR after a
/// `changed()` event lands a terminal value. Yields control on
/// every iteration so the timeout future in
/// [`BgAgentRegistry::wait_for_completion`] gets a chance to fire.
async fn wait_for_terminal_status(mut rx: watch::Receiver<AgentStatus>) {
    loop {
        let is_terminal = matches!(
            *rx.borrow(),
            AgentStatus::Completed { .. } | AgentStatus::Errored { .. } | AgentStatus::Cancelled
        );
        if is_terminal {
            return;
        }
        // `changed()` resolves on the next write to the channel. If
        // the sender was dropped (task panicked) it returns Err —
        // treat as terminal so the caller can pull `Cancelled` from
        // the closed oneshot.
        if rx.changed().await.is_err() {
            return;
        }
    }
}

impl Default for BgAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BgAgentRegistry {
    /// Abort every still-pending bg task on registry drop.
    ///
    /// `AbortOnDropHandle::drop` does the work — this impl exists
    /// only to make the lifecycle explicit and to give a single
    /// place to add telemetry later.
    fn drop(&mut self) {
        // **#1022 B16**: simplified post-parking_lot. The pre-fix
        // version had to handle `PoisonError` (via
        // `match get_mut() { Ok | Err(into_inner()) }`) because a
        // panic-while-held would poison `std::sync::Mutex`.
        // `parking_lot::Mutex` doesn't poison, so the cleanup path
        // is now the obvious one: take the map, log if non-empty,
        // let `AbortOnDropHandle::drop` do the actual abort work.
        let map = std::mem::take(&mut *self.pending.lock());
        if !map.is_empty() {
            tracing::debug!(
                count = map.len(),
                "BgAgentRegistry dropped with pending tasks; aborting"
            );
        }
        // Map drops here → each entry's `AbortOnDropHandle` aborts
        // its task. No orphans. No leaked worktrees.
    }
}

/// Wrap in Arc for sharing between inference loop and tool dispatch.
pub fn new_shared() -> Arc<BgAgentRegistry> {
    Arc::new(BgAgentRegistry::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn register_and_complete() {
        let reg = BgAgentRegistry::new();
        let (task_id, tx) = reg.register_test("explore", "find all tests");
        assert_eq!(task_id, 1);
        assert_eq!(reg.pending_count(), 1);

        // Not yet complete
        assert!(reg.drain_completed().is_empty());

        // Complete it
        tx.send(Ok(("found 42 tests".to_string(), Vec::new())))
            .unwrap();
        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_name, "explore");
        assert_eq!(results[0].output, "found 42 tests");
        assert!(results[0].success);
        assert_eq!(reg.pending_count(), 0);
    }

    #[tokio::test]
    async fn drain_only_completed() {
        let reg = BgAgentRegistry::new();
        let (_id1, tx1) = reg.register_test("task", "build");
        let (_id2, _tx2) = reg.register_test("explore", "search");

        tx1.send(Ok(("done".to_string(), Vec::new()))).unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_name, "task");
        assert_eq!(reg.pending_count(), 1); // explore still pending
    }

    #[tokio::test]
    async fn dropped_sender_reports_cancelled() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("task", "build");
        drop(tx); // simulate task panic/cancel

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(results[0].output.contains("cancelled"));
    }

    #[tokio::test]
    async fn error_result() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("verify", "check");
        tx.send(Err(("test failures".to_string(), Vec::new())))
            .unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].output, "test failures");
    }

    /// #1022 B9 regression: the narrative trace captured by
    /// `BufferingSink` inside the bg agent must propagate through
    /// the oneshot → registry → `BgAgentResult.events`. Pre-fix
    /// this field didn't exist; bg agents ran with `NullSink` and
    /// the user only saw spawn + completion lines. The fix is
    /// useless if the trace gets dropped at any of the three hops,
    /// so this test pins the round-trip end-to-end.
    #[tokio::test]
    async fn events_propagate_through_drain_for_success() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("explore", "map repo");
        let trace = vec![
            "  \u{1f527} Read".to_string(),
            "  \u{1f527} Grep".to_string(),
            "  \u{26a1} cache hit".to_string(),
        ];
        tx.send(Ok(("map result".to_string(), trace.clone())))
            .unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert_eq!(
            results[0].events, trace,
            "trace lost between sender and BgAgentResult"
        );
    }

    /// #1022 B9 regression: trace must propagate even when the bg
    /// agent failed. The trace is *most* useful in the failure case
    /// — "the agent tried Read, Bash, Edit, then errored" is the
    /// kind of breadcrumb that turns a black-box failure into a
    /// debuggable one.
    #[tokio::test]
    async fn events_propagate_through_drain_for_failure() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("build", "compile");
        let trace = vec![
            "  \u{1f527} Bash".to_string(),
            "  \u{2398} approval auto-rejected for Delete (no user channel)".to_string(),
        ];
        tx.send(Err(("compile failed".to_string(), trace.clone())))
            .unwrap();

        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].events, trace);
    }

    /// #1022 B9 corollary: cancelled / panicked tasks have *no*
    /// trace available (the buffering sink died with the task), and
    /// that's an explicitly-empty Vec rather than uninitialized.
    #[tokio::test]
    async fn cancelled_task_has_empty_event_trace() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("flaky", "x");
        drop(tx); // simulate panic / abort
        let results = reg.drain_completed();
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert!(
            results[0].events.is_empty(),
            "cancel path must yield empty trace"
        );
    }

    /// Phase 1 of #1022, B3 regression test: dropping the registry
    /// must abort still-running spawned tasks. Without
    /// `AbortOnDropHandle` (or an explicit `JoinHandle::abort` in
    /// `Drop`), the spawned future would keep running after the
    /// registry — and any worktrees / API tokens / writes it owns —
    /// were dropped. That's the leak we're fixing.
    #[tokio::test]
    async fn registry_drop_aborts_pending_tasks() {
        let reg = BgAgentRegistry::new();
        let parent = CancellationToken::new();
        let reservation = reg.reserve(&parent, None);
        let task_id = reservation.task_id;
        let cancel_for_task = reservation.cancel.clone();
        let tx = reservation.tx;
        let rx = reservation.rx;
        let cancel_for_entry = reservation.cancel;
        let status_rx = reservation.status_rx;

        // Use a flag the task sets only if it ever finishes a full
        // sleep. If abort works, the flag stays false even though
        // we wait long enough for a non-aborted task to finish.
        let ran_to_completion = Arc::new(AtomicBool::new(false));
        let flag = ran_to_completion.clone();
        let handle = tokio::spawn(async move {
            // Either the cancel token fires (parent cascade) or we
            // get aborted (drop cascade). The slow sleep just gives
            // the test time to drop the registry before we'd
            // naturally finish.
            tokio::select! {
                _ = cancel_for_task.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            let _ = tx.send(Ok(("done".to_string(), Vec::new())));
        });
        reg.attach(
            task_id,
            "explore",
            "long task",
            rx,
            cancel_for_entry,
            status_rx,
            None,
            handle,
        );

        // Give the task a tick to start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(reg.pending_count(), 1);

        // Drop the registry — this must abort the spawned task.
        drop(reg);

        // Yield long enough for the abort to land; well under the
        // 60 s sleep the task would have completed otherwise.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !ran_to_completion.load(Ordering::SeqCst),
            "task slept to completion — AbortOnDropHandle did not abort it"
        );
    }

    /// Phase 1 of #1022, B2 regression test: cancelling the parent
    /// token must cascade to bg-agent child tokens handed out by
    /// `reserve`.
    #[tokio::test]
    async fn parent_cancel_cascades_to_reserved_child() {
        let reg = BgAgentRegistry::new();
        let parent = CancellationToken::new();
        let r1 = reg.reserve(&parent, None);
        let r2 = reg.reserve(&parent, None);

        assert!(!r1.cancel.is_cancelled());
        assert!(!r2.cancel.is_cancelled());

        parent.cancel();

        assert!(
            r1.cancel.is_cancelled(),
            "child 1 token should observe parent cancel"
        );
        assert!(
            r2.cancel.is_cancelled(),
            "child 2 token should observe parent cancel"
        );
    }

    // ── Layer 0 of #996 ──────────────────────────────────────────────────────
    //
    // Status channel + per-task cancel + snapshot.

    /// `cancel(task_id)` must fire that task's cancel token.
    /// This is the hook the future `/cancel <id>` slash command and
    /// `CancelAgent` LLM tool will call. Verifies a known id returns
    /// true *and* the underlying token actually fires.
    #[tokio::test]
    async fn cancel_known_task_fires_token() {
        let reg = BgAgentRegistry::new();
        let (task_id, _tx, _status_tx, observer) =
            reg.register_test_with_status("explore", "map repo", None);

        assert!(!observer.is_cancelled(), "precondition");
        let fired = reg.cancel(task_id);
        assert!(fired, "cancel(known_id) should report success");
        assert!(
            observer.is_cancelled(),
            "the task's cancel token should observe the cancellation"
        );
    }

    /// `cancel` on an unknown / already-drained id must return false
    /// instead of panicking. The slash command and LLM tool will
    /// surface this to the user as "no such task".
    #[tokio::test]
    async fn cancel_unknown_task_returns_false() {
        let reg = BgAgentRegistry::new();
        assert!(
            !reg.cancel(999),
            "cancel of an unknown id should be a no-op returning false"
        );
    }

    /// `cancel` is idempotent — calling twice on the same id is safe
    /// (the underlying [`CancellationToken::cancel`] is itself
    /// idempotent). Both calls return true while the entry is still
    /// in `pending`; a third call after drain returns false.
    #[tokio::test]
    async fn cancel_is_idempotent_while_pending() {
        let reg = BgAgentRegistry::new();
        let (task_id, _tx, _status_tx, _observer) =
            reg.register_test_with_status("explore", "x", None);

        assert!(reg.cancel(task_id));
        assert!(
            reg.cancel(task_id),
            "second cancel should still find the entry and report success"
        );
    }

    /// `snapshot()` must return one entry per pending task with
    /// stable ordering by `task_id`. Status defaults to `Pending`
    /// because no spawned future has flipped it yet.
    #[tokio::test]
    async fn snapshot_lists_pending_tasks_in_id_order() {
        let reg = BgAgentRegistry::new();
        let (id_a, _tx_a) = reg.register_test("explore", "map");
        let (id_b, _tx_b) = reg.register_test("verify", "check");

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 2);
        // Ordering is by ascending task_id, regardless of HashMap
        // iteration order — this is the contract `/agents` relies on.
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
    /// This is the contract that lets the status-bar pill (Layer 3)
    /// and live `/agents -v` (Layer 1) reflect transitions immediately.
    #[tokio::test]
    async fn snapshot_reflects_status_writes() {
        let reg = BgAgentRegistry::new();
        let (task_id, _tx, status_tx, _cancel) =
            reg.register_test_with_status("explore", "map", None);

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
    #[tokio::test]
    async fn snapshot_age_is_monotonic() {
        let reg = BgAgentRegistry::new();
        let (_id, _tx) = reg.register_test("explore", "x");

        let age1 = reg.snapshot()[0].age;
        tokio::time::sleep(Duration::from_millis(15)).await;
        let age2 = reg.snapshot()[0].age;
        assert!(
            age2 >= age1,
            "age should be monotonic non-decreasing across snapshots"
        );
    }

    /// `snapshot()` on an empty registry returns an empty Vec, not a
    /// panic and not None. `/agents` will use this to render "No
    /// background agents."
    #[tokio::test]
    async fn snapshot_empty_registry_is_empty_vec() {
        let reg = BgAgentRegistry::new();
        assert!(reg.snapshot().is_empty());
    }

    /// Once a task is drained (completed and removed from `pending`),
    /// it disappears from `snapshot()` immediately. This pins the
    /// contract that `/agents` reflects the *currently-pending* set,
    /// not historical tasks. The Layer 1 "recently-completed lingers
    /// 30 s" UX is implemented at the *display* layer, not here.
    #[tokio::test]
    async fn snapshot_drops_drained_tasks() {
        let reg = BgAgentRegistry::new();
        let (_id, tx) = reg.register_test("explore", "x");
        assert_eq!(reg.snapshot().len(), 1);

        tx.send(Ok(("done".to_string(), Vec::new()))).unwrap();
        let _ = reg.drain_completed();

        assert!(
            reg.snapshot().is_empty(),
            "drained tasks must not appear in snapshots"
        );
    }

    // ── Layer 2 of #996: scoped APIs + WaitOutcome ────────────────────────

    /// `snapshot_for_caller(None)` returns only top-level tasks;
    /// `snapshot_for_caller(Some(N))` returns only N's tasks. Cross-spawner
    /// visibility is exactly zero — the Model E isolation guarantee.
    #[tokio::test]
    async fn snapshot_for_caller_filters_by_spawner() {
        let reg = BgAgentRegistry::new();
        let (top_id, _tx, _, _) = reg.register_test_with_status("a", "top", None);
        let (sub_a_id, _tx, _, _) = reg.register_test_with_status("b", "sub-a", Some(7));
        let (_sub_b_id, _tx, _, _) = reg.register_test_with_status("c", "sub-b", Some(9));

        let top = reg.snapshot_for_caller(None);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].task_id, top_id);

        let sub_a = reg.snapshot_for_caller(Some(7));
        assert_eq!(sub_a.len(), 1);
        assert_eq!(sub_a[0].task_id, sub_a_id);

        // Sub-agent 7 sees nothing of sub-agent 9's, of top-level's, etc.
        assert!(reg.snapshot_for_caller(Some(42)).is_empty());
    }

    /// `cancel_as_caller` enforces the Model E permission rule.
    #[tokio::test]
    async fn cancel_as_caller_returns_forbidden_for_other_spawner() {
        let reg = BgAgentRegistry::new();
        let (id, _tx, _, observer) = reg.register_test_with_status("x", "y", Some(7));

        // Wrong caller — not the top-level (None != Some(7)) and not a peer.
        assert_eq!(
            reg.cancel_as_caller(id, None),
            CancelOutcome::Forbidden,
            "top-level must not be able to cancel sub-agent's task"
        );
        assert_eq!(
            reg.cancel_as_caller(id, Some(99)),
            CancelOutcome::Forbidden,
            "sibling sub-agent must not be able to cancel"
        );
        assert!(
            !observer.is_cancelled(),
            "forbidden calls must NOT fire the cancel token"
        );

        // Correct caller — the original spawner.
        assert_eq!(reg.cancel_as_caller(id, Some(7)), CancelOutcome::Cancelled);
        assert!(observer.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_as_caller_returns_not_found_for_unknown_id() {
        let reg = BgAgentRegistry::new();
        assert_eq!(reg.cancel_as_caller(999, None), CancelOutcome::NotFound);
    }

    /// `cancel_for_spawner` cleans up exactly one sub-agent's children
    /// and leaves siblings + top-level alone. The cleanup-on-exit hook.
    #[tokio::test]
    async fn cancel_for_spawner_kills_only_matching_children() {
        let reg = BgAgentRegistry::new();
        let (_top, _, _, top_obs) = reg.register_test_with_status("top", "t", None);
        let (_a1, _, _, a1_obs) = reg.register_test_with_status("a1", "x", Some(7));
        let (_a2, _, _, a2_obs) = reg.register_test_with_status("a2", "y", Some(7));
        let (_b, _, _, b_obs) = reg.register_test_with_status("b", "z", Some(9));

        let count = reg.cancel_for_spawner(7);
        assert_eq!(count, 2, "both of spawner 7's children must be signalled");

        assert!(a1_obs.is_cancelled());
        assert!(a2_obs.is_cancelled());
        assert!(!top_obs.is_cancelled(), "top-level must be untouched");
        assert!(!b_obs.is_cancelled(), "sibling spawner's task untouched");

        // Idempotent — calling again after entries are still alive
        // re-fires the (already-cancelled, no-op) tokens.
        assert_eq!(reg.cancel_for_spawner(7), 2);
        // Calling for an unknown spawner returns 0.
        assert_eq!(reg.cancel_for_spawner(99), 0);
    }

    /// `wait_for_completion` returns `Completed` and consumes the entry
    /// (so a subsequent `drain_completed` can't double-inject).
    #[tokio::test]
    async fn wait_for_completion_consumes_completed_task() {
        let reg = BgAgentRegistry::new();
        let (id, tx, status_tx, _) = reg.register_test_with_status("explore", "map", Some(3));

        // Fire the result, then transition status to terminal so the
        // wait future wakes up.
        tx.send(Ok(("final answer".to_string(), vec!["step 1".to_string()])))
            .unwrap();
        status_tx
            .send(AgentStatus::Completed {
                summary: "final answer".to_string(),
            })
            .unwrap();

        let outcome = reg
            .wait_for_completion(id, Some(3), Duration::from_secs(1))
            .await;
        match outcome {
            WaitOutcome::Completed(result) => {
                assert!(result.success);
                assert_eq!(result.output, "final answer");
                assert_eq!(result.events, vec!["step 1".to_string()]);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Entry must be gone — drain sees nothing.
        assert_eq!(reg.drain_completed().len(), 0);
        assert!(reg.snapshot().is_empty());
    }

    /// `wait_for_completion` returns `TimedOut` with a fresh snapshot
    /// when the task hasn't finished yet — and crucially leaves the
    /// entry in the registry so a later drain still works.
    #[tokio::test]
    async fn wait_for_completion_timeout_preserves_entry() {
        let reg = BgAgentRegistry::new();
        let (id, _tx, status_tx, _) = reg.register_test_with_status("slow", "x", None);

        // Move to Running so the snapshot test below can verify the
        // current status got carried through.
        status_tx.send(AgentStatus::Running { iter: 2 }).unwrap();

        let outcome = reg
            .wait_for_completion(id, None, Duration::from_millis(40))
            .await;
        match outcome {
            WaitOutcome::TimedOut(snap) => {
                assert_eq!(snap.task_id, id);
                assert_eq!(snap.status, AgentStatus::Running { iter: 2 });
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        // Still in pending after a timeout.
        assert_eq!(reg.snapshot().len(), 1);
    }

    /// `wait_for_completion` enforces the same Model E permission rule
    /// as `cancel_as_caller`.
    #[tokio::test]
    async fn wait_for_completion_returns_forbidden_for_other_spawner() {
        let reg = BgAgentRegistry::new();
        let (id, _tx, _, _) = reg.register_test_with_status("x", "y", Some(5));

        let outcome = reg
            .wait_for_completion(id, None, Duration::from_millis(20))
            .await;
        assert!(
            matches!(outcome, WaitOutcome::Forbidden),
            "top-level must not be able to wait on sub-agent task; got {outcome:?}"
        );

        let outcome = reg
            .wait_for_completion(id, Some(99), Duration::from_millis(20))
            .await;
        assert!(
            matches!(outcome, WaitOutcome::Forbidden),
            "sibling sub-agent must not be able to wait; got {outcome:?}"
        );
    }

    /// Cancellation between status going terminal and `WaitTask` waking
    /// up surfaces as `Cancelled` (oneshot closed without sending).
    #[tokio::test]
    async fn wait_for_completion_returns_cancelled_when_sender_dropped() {
        let reg = BgAgentRegistry::new();
        let (id, tx, status_tx, _) = reg.register_test_with_status("x", "y", None);

        // Drop the sender (simulates task panic / abort), then push
        // the status to terminal so wait wakes up.
        drop(tx);
        status_tx.send(AgentStatus::Cancelled).unwrap();

        let outcome = reg
            .wait_for_completion(id, None, Duration::from_secs(1))
            .await;
        assert!(matches!(outcome, WaitOutcome::Cancelled), "got {outcome:?}");
        assert!(reg.snapshot().is_empty(), "entry must be reaped");
    }

    #[tokio::test]
    async fn wait_for_completion_returns_not_found_for_unknown_id() {
        let reg = BgAgentRegistry::new();
        let outcome = reg
            .wait_for_completion(999, None, Duration::from_millis(10))
            .await;
        assert!(matches!(outcome, WaitOutcome::NotFound), "got {outcome:?}");
    }

    /// Regression test for the PR #1043 race fix.
    ///
    /// Scenario: the bg future writes terminal `AgentStatus` to the
    /// watch channel, then — after a yield-induced gap — sends the
    /// payload on the oneshot. The waiter is woken on the watch
    /// notify and races to read the oneshot.
    ///
    /// Before the fix, `wait_for_completion` did `try_recv()` twice
    /// back-to-back with no `await` between them; on a multi-thread
    /// runtime the second `try_recv` could observe `Empty` again and
    /// return `Cancelled` for a successful task. The fix awaits the
    /// oneshot future directly with a short inner timeout, which
    /// gives the sender's task a chance to run.
    ///
    /// Multi-thread runtime + explicit `yield_now` between the two
    /// sends reliably triggers the old race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_completion_handles_status_then_yield_then_payload() {
        let reg = Arc::new(BgAgentRegistry::new());
        let (id, tx, status_tx, _observer) =
            reg.register_test_with_status("explore", "map repo", None);

        // Spawn a task that mimics `run_bg_agent`'s send ordering:
        // status first, yield, payload. The yield is what exposed the
        // race — it forces tokio to potentially schedule the waiter
        // between the two sends.
        let send_task = tokio::spawn(async move {
            status_tx
                .send(AgentStatus::Completed {
                    summary: "done".into(),
                })
                .unwrap();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            let _ = tx.send(Ok(("final".into(), vec!["e1".into()])));
        });

        let outcome = reg
            .wait_for_completion(id, None, Duration::from_secs(2))
            .await;
        send_task.await.unwrap();

        match outcome {
            WaitOutcome::Completed(result) => {
                assert_eq!(result.output, "final");
                assert!(result.success);
                assert_eq!(result.events, vec!["e1".to_string()]);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
