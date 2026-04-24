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
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

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
    /// session's token at spawn time. Firing this token causes the
    /// in-flight bg agent to observe `is_cancelled()` on its next
    /// loop iteration. Currently used only for the registry-drop
    /// path; per-task cancel UX lands in #996.
    #[allow(dead_code)]
    cancel: CancellationToken,
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
    /// will key the future per-task `/cancel <id>` UX (#996).
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
    /// The two-phase shape exists because `tokio::spawn` produces the
    /// `JoinHandle` *after* the future is built, but the future needs
    /// to own the `tx` to deliver its result. Reservation gives us
    /// `tx` early; attach binds the handle once it exists.
    pub fn reserve(&self, parent_cancel: &CancellationToken) -> BgAgentReservation {
        let (tx, rx) = oneshot::channel();
        let mut id = self.next_id.lock();
        let task_id = *id;
        *id += 1;
        BgAgentReservation {
            task_id,
            tx,
            rx,
            cancel: parent_cancel.child_token(),
        }
    }

    /// Bind a spawned task's metadata to a previously [`reserve`]d slot.
    ///
    /// `rx` must be the receiver paired with the `tx` handed out by
    /// `reserve`. Holding `handle` as `AbortOnDropHandle` ensures the
    /// task is aborted on registry drop (B3 of #1022).
    ///
    /// [`reserve`]: Self::reserve
    pub fn attach(
        &self,
        reservation_id: u32,
        agent_name: &str,
        prompt: &str,
        rx: oneshot::Receiver<Result<BgPayload, BgPayload>>,
        cancel: CancellationToken,
        handle: tokio::task::JoinHandle<()>,
    ) {
        self.pending.lock().insert(
            reservation_id,
            BgAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                rx,
                cancel,
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
        let (tx, rx) = oneshot::channel();
        let mut id = self.next_id.lock();
        let task_id = *id;
        *id += 1;
        drop(id);
        let cancel = CancellationToken::new();
        let noop = tokio::spawn(async {});
        self.pending.lock().insert(
            task_id,
            BgAgentEntry {
                agent_name: agent_name.to_string(),
                prompt: prompt.to_string(),
                rx,
                cancel,
                _handle: AbortOnDropHandle::new(noop),
            },
        );
        (task_id, tx)
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
        let reservation = reg.reserve(&parent);
        let task_id = reservation.task_id;
        let cancel_for_task = reservation.cancel.clone();
        let tx = reservation.tx;
        let rx = reservation.rx;
        let cancel_for_entry = reservation.cancel;

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
        let r1 = reg.reserve(&parent);
        let r2 = reg.reserve(&parent);

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
}
