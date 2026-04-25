//! Background process registry.
//!
//! Tracks processes spawned by `Bash { background: true }` so they can be
//! listed, waited on, killed, and cleaned up (SIGTERM) when the session ends
//! or the spawning sub-agent exits (Model E, see #996).
//!
//! ## Usage
//!
//! ```text
//! Model calls: Bash { command: "npm run dev", background: true }
//!   → Process spawned, PID + spawner recorded in BgRegistry
//!   → Tool returns immediately: "Started PID 12345"
//!   → Model continues with other work
//!   → On session end: all tracked PIDs receive SIGTERM
//!   → On spawning sub-agent exit: kill_for_spawner reaps that
//!     sub-agent's processes (Model E cleanup-on-exit)
//! ```
//!
//! ## Status lifecycle
//!
//! Each tracked process has a [`BgProcessStatus`] that transitions
//! `Running` → `Exited { code }` (natural exit) or `Running` → `Killed`
//! (we sent SIGTERM). [`BgRegistry::reap`] is the sole writer of the
//! `Exited` transition; it calls `try_wait` on every still-running child
//! and updates status without blocking. The TUI / LLM tool layers call
//! `reap()` before producing a snapshot so the displayed status is fresh.
//!
//! Entries persist past terminal status so the LLM can observe the exit
//! code via `ListBackgroundTasks` / `WaitTask`. Manual purge is the
//! caller's job for now (sessions are short; auto-purge can come later).
//!
//! ## Design
//!
//! Each `ToolRegistry` owns one `BgRegistry`. All background processes for
//! the session are keyed by PID. The registry is `Mutex`-protected since
//! the spawning thread, the reaper, and the cleanup path all touch it.

use crate::bg_agent::CancelOutcome;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Child;

/// Outcome of [`BgRegistry::wait_for_exit_as_caller`].
///
/// Mirrors [`crate::bg_agent::WaitOutcome`] but carries process-specific
/// terminal info (exit code) instead of an agent result. The two enums
/// stay separate because they really are different things — forcing one
/// to wear the other's shape would mean optionalizing fields that aren't
/// optional in their natural domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessWaitOutcome {
    /// Process has exited — either naturally or as a result of an
    /// earlier `kill`. `code` is the OS exit code if reported.
    Exited {
        /// Same semantics as [`BgProcessStatus::Exited::code`].
        code: Option<i32>,
    },
    /// Wait deadline elapsed; the process is still running. The
    /// returned snapshot reflects the latest state at the moment the
    /// timeout fired (e.g. age has advanced).
    TimedOut(BgProcessSnapshot),
    /// PID is not in the registry (never tracked, or already removed).
    NotFound,
    /// PID exists but caller's `spawner` does not match. Model E
    /// permission rule.
    Forbidden,
}

/// Lifecycle of a single tracked background process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BgProcessStatus {
    /// Child process is still alive (last `try_wait` returned `Ok(None)`).
    Running,
    /// Child has exited naturally. `code` is the OS exit code if the
    /// platform reported one (POSIX always does for normal exits;
    /// signal-killed processes report `None` on most platforms).
    Exited {
        /// Process exit code as reported by the OS, or `None` for
        /// signal-killed (POSIX returns no code in that case).
        code: Option<i32>,
    },
    /// We sent SIGTERM via [`BgRegistry::kill`] / [`BgRegistry::kill_as_caller`].
    /// The child may still be alive briefly; the reaper transitions it
    /// to `Exited` once it's actually gone.
    Killed,
}

/// Snapshot of a tracked background process — what `/agents` (combined
/// view, see #996 Layer 2) and the `ListBackgroundTasks` LLM tool render.
///
/// Cloned out of the registry under the lock so callers can format /
/// display without holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgProcessSnapshot {
    /// OS process id. Stable for the process's lifetime; reused by
    /// the kernel after it exits (so don't compare snapshots across
    /// long pauses).
    pub pid: u32,
    /// The original shell command string. Surfaced verbatim by the
    /// TUI; truncation is the renderer's job.
    pub command: String,
    /// Wall-clock duration since insert. Computed at snapshot time,
    /// so successive snapshots of the same process report different
    /// ages.
    pub age: Duration,
    /// Latest known status (set by [`BgRegistry::reap`] / `kill`).
    pub status: BgProcessStatus,
    /// Sub-agent invocation id of the spawner, or `None` for the
    /// top-level inference loop. Drives [`BgRegistry::kill_for_spawner`]
    /// (Model E cleanup-on-exit) and the LLM scope-filter.
    pub spawner: Option<u32>,
}

/// Metadata stored alongside the child handle.
struct BgEntry {
    /// The original shell command string.
    command: String,
    /// The spawned child process handle (used for `start_kill` on
    /// kill-paths and `try_wait` on reap).
    child: Child,
    /// When the entry was inserted. Drives `age` in snapshots.
    started_at: Instant,
    /// Current status. Only [`BgRegistry::reap`] and the kill-paths
    /// transition this away from `Running`.
    status: BgProcessStatus,
    /// Sub-agent that spawned this process. `None` = top-level.
    spawner: Option<u32>,
}

/// Registry of running background processes, scoped to one session.
///
/// Drop kills all remaining processes (SIGTERM).
pub struct BgRegistry {
    inner: Mutex<HashMap<u32, BgEntry>>,
}

impl BgRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Register a spawned child. `spawner` is the sub-agent invocation
    /// id (`None` for top-level). Returns the PID.
    pub fn insert(&self, pid: u32, command: String, child: Child, spawner: Option<u32>) -> u32 {
        self.inner.lock().unwrap().insert(
            pid,
            BgEntry {
                command,
                child,
                started_at: Instant::now(),
                status: BgProcessStatus::Running,
                spawner,
            },
        );
        pid
    }

    /// Return a snapshot of running PIDs + commands for display.
    ///
    /// **Legacy path** kept for the TUI's existing `/agents` rendering
    /// (#1042). New code should prefer [`Self::snapshot`] which carries
    /// status, age, and spawner.
    pub fn list(&self) -> Vec<(u32, String)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(pid, e)| (*pid, e.command.clone()))
            .collect()
    }

    /// Snapshot every tracked process for `/agents` and the
    /// `ListBackgroundTasks` LLM tool. Sorted by ascending PID.
    ///
    /// **Unscoped**: returns every entry regardless of spawner. Used by
    /// the TUI (humans get the global view) and as the engine of
    /// [`Self::snapshot_for_caller`].
    pub fn snapshot(&self) -> Vec<BgProcessSnapshot> {
        let guard = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut out: Vec<_> = guard
            .iter()
            .map(|(pid, e)| BgProcessSnapshot {
                pid: *pid,
                command: e.command.clone(),
                age: now.saturating_duration_since(e.started_at),
                status: e.status,
                spawner: e.spawner,
            })
            .collect();
        out.sort_by_key(|s| s.pid);
        out
    }

    /// Scoped snapshot for the `ListBackgroundTasks` LLM tool. Same
    /// Model E rule as [`crate::bg_agent::BgAgentRegistry::snapshot_for_caller`]:
    /// strict spawner equality, `None == None`.
    pub fn snapshot_for_caller(&self, caller_spawner: Option<u32>) -> Vec<BgProcessSnapshot> {
        self.snapshot()
            .into_iter()
            .filter(|s| s.spawner == caller_spawner)
            .collect()
    }

    /// How many processes are currently tracked (any status).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Returns `true` if no background processes are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// Non-blocking poll on every still-`Running` child. Transitions
    /// any that have exited to `Exited { code }` so subsequent
    /// snapshots see the fresh status.
    ///
    /// Cheap: zero syscalls if the registry is empty; one `waitpid`
    /// per running child otherwise. Safe to call before every
    /// `snapshot()` (the TUI does so via the slash command path).
    pub fn reap(&self) {
        let mut guard = self.inner.lock().unwrap();
        for entry in guard.values_mut() {
            if entry.status != BgProcessStatus::Running {
                continue;
            }
            match entry.child.try_wait() {
                Ok(Some(exit)) => {
                    entry.status = BgProcessStatus::Exited { code: exit.code() };
                }
                Ok(None) => { /* still running */ }
                Err(e) => {
                    // try_wait can fail if the OS lost track (rare).
                    // Log + treat as terminal so we don't spin.
                    tracing::warn!("BgRegistry reap try_wait failed for PID {}: {e}", entry
                        .child
                        .id()
                        .unwrap_or(0));
                    entry.status = BgProcessStatus::Exited { code: None };
                }
            }
        }
    }

    /// Send SIGTERM to a tracked PID. Returns `true` if the PID was
    /// known (whether or not the kill succeeded — the underlying error
    /// is logged).
    ///
    /// Status flips to `Killed` immediately; the reaper will surface
    /// the eventual `Exited { code }` once the child is reaped by the
    /// kernel. **Unscoped** — TUI `/cancel` contract; LLM goes through
    /// [`Self::kill_as_caller`].
    pub fn kill(&self, pid: u32) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let Some(entry) = guard.get_mut(&pid) else {
            return false;
        };
        if entry.status == BgProcessStatus::Running {
            if let Err(e) = entry.child.start_kill() {
                tracing::warn!("BgRegistry::kill: failed to SIGTERM PID {pid}: {e}");
            }
            entry.status = BgProcessStatus::Killed;
        }
        true
    }

    /// Scoped kill for the `CancelTask` LLM tool. Same Model E rule
    /// as [`crate::bg_agent::BgAgentRegistry::cancel_as_caller`].
    pub fn kill_as_caller(&self, pid: u32, caller_spawner: Option<u32>) -> CancelOutcome {
        let mut guard = self.inner.lock().unwrap();
        let Some(entry) = guard.get_mut(&pid) else {
            return CancelOutcome::NotFound;
        };
        if entry.spawner != caller_spawner {
            return CancelOutcome::Forbidden;
        }
        if entry.status == BgProcessStatus::Running {
            if let Err(e) = entry.child.start_kill() {
                tracing::warn!("BgRegistry::kill_as_caller: SIGTERM PID {pid}: {e}");
            }
            entry.status = BgProcessStatus::Killed;
        }
        CancelOutcome::Cancelled
    }

    /// Block until a tracked process exits, with a timeout. Same
    /// Model E permission rule as [`Self::kill_as_caller`].
    ///
    /// Implementation: poll-based. Calls [`Self::reap`] every
    /// `POLL_INTERVAL`; cheap because reap is a `try_wait` per
    /// running child. Once status leaves `Running`, returns
    /// [`ProcessWaitOutcome::Exited`] (mapping `Killed` → `Exited`
    /// with `code: None` if the OS hasn't reported the exit yet —
    /// the model only cares that the process is gone).
    ///
    /// On timeout, leaves the entry in the registry so it can still
    /// be queried via [`Self::snapshot`] / re-waited.
    pub async fn wait_for_exit_as_caller(
        &self,
        pid: u32,
        caller_spawner: Option<u32>,
        timeout: Duration,
    ) -> ProcessWaitOutcome {
        const POLL_INTERVAL: Duration = Duration::from_millis(100);

        // Sanity-check: known + owned. Done up-front so we can fail
        // fast on the common error cases without spinning.
        {
            let guard = self.inner.lock().unwrap();
            match guard.get(&pid) {
                None => return ProcessWaitOutcome::NotFound,
                Some(e) if e.spawner != caller_spawner => return ProcessWaitOutcome::Forbidden,
                Some(_) => {}
            }
        }

        let deadline = Instant::now() + timeout;
        loop {
            self.reap();
            {
                let guard = self.inner.lock().unwrap();
                let Some(entry) = guard.get(&pid) else {
                    return ProcessWaitOutcome::NotFound;
                };
                match entry.status {
                    BgProcessStatus::Running => {}
                    BgProcessStatus::Exited { code } => {
                        return ProcessWaitOutcome::Exited { code };
                    }
                    BgProcessStatus::Killed => {
                        return ProcessWaitOutcome::Exited { code: None };
                    }
                }
            }

            if Instant::now() >= deadline {
                let guard = self.inner.lock().unwrap();
                let Some(entry) = guard.get(&pid) else {
                    return ProcessWaitOutcome::NotFound;
                };
                let now = Instant::now();
                return ProcessWaitOutcome::TimedOut(BgProcessSnapshot {
                    pid,
                    command: entry.command.clone(),
                    age: now.saturating_duration_since(entry.started_at),
                    status: entry.status,
                    spawner: entry.spawner,
                });
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }

    /// SIGTERM every still-running child whose `spawner` matches.
    /// Cleanup-on-exit hook for sub-agent exit (Model E). Returns
    /// the count of processes signalled. Idempotent.
    pub fn kill_for_spawner(&self, spawner: u32) -> usize {
        let mut guard = self.inner.lock().unwrap();
        let mut count = 0;
        for entry in guard.values_mut() {
            if entry.spawner != Some(spawner) {
                continue;
            }
            if entry.status == BgProcessStatus::Running {
                if let Err(e) = entry.child.start_kill() {
                    tracing::warn!(
                        "BgRegistry::kill_for_spawner: SIGTERM PID {}: {e}",
                        entry.child.id().unwrap_or(0)
                    );
                }
                entry.status = BgProcessStatus::Killed;
                count += 1;
            }
        }
        count
    }
}

impl Default for BgRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BgRegistry {
    /// Best-effort SIGTERM all still-running tracked processes when
    /// the session ends.
    fn drop(&mut self) {
        let mut guard = self.inner.lock().unwrap();
        for (pid, entry) in guard.iter_mut() {
            if entry.status != BgProcessStatus::Running {
                continue;
            }
            if let Err(e) = entry.child.start_kill() {
                tracing::warn!("BgRegistry drop: failed to kill PID {pid}: {e}");
            } else {
                tracing::debug!("BgRegistry drop: sent SIGTERM to PID {pid}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_sleep_child() -> (u32, Child) {
        // 60s sleep gives every test plenty of headroom; we either kill
        // it explicitly or let Drop SIGTERM it.
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid");
        (pid, child)
    }

    fn spawn_true_child() -> (u32, Child) {
        let child = tokio::process::Command::new("true").spawn().expect("spawn");
        let pid = child.id().unwrap_or(99999);
        (pid, child)
    }

    #[test]
    fn registry_starts_empty() {
        let reg = BgRegistry::new();
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.snapshot().is_empty());
    }

    #[tokio::test]
    async fn insert_records_spawner_and_appears_in_snapshot() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, Some(7));

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].pid, pid);
        assert_eq!(snap[0].command, "sleep 60");
        assert_eq!(snap[0].status, BgProcessStatus::Running);
        assert_eq!(snap[0].spawner, Some(7));
    }

    #[tokio::test]
    async fn snapshot_for_caller_filters_by_spawner() {
        let reg = BgRegistry::new();
        let (p1, c1) = spawn_sleep_child();
        let (p2, c2) = spawn_sleep_child();
        let (p3, c3) = spawn_sleep_child();
        reg.insert(p1, "a".into(), c1, None);
        reg.insert(p2, "b".into(), c2, Some(7));
        reg.insert(p3, "c".into(), c3, Some(9));

        let top = reg.snapshot_for_caller(None);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].pid, p1);

        let sub_7 = reg.snapshot_for_caller(Some(7));
        assert_eq!(sub_7.len(), 1);
        assert_eq!(sub_7[0].pid, p2);

        // Sibling sees nothing of peer's.
        assert!(reg.snapshot_for_caller(Some(42)).is_empty());
    }

    #[tokio::test]
    async fn reap_transitions_finished_children_to_exited() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_true_child();
        reg.insert(pid, "true".into(), child, None);

        // `true` exits ~immediately, but tokio's try_wait needs time
        // for the SIGCHLD handler to register the exit. Poll up to 1s.
        let mut observed = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            reg.reap();
            let snap = reg.snapshot();
            if let BgProcessStatus::Exited { code } = snap[0].status {
                observed = Some(code);
                break;
            }
        }
        assert_eq!(
            observed,
            Some(Some(0)),
            "reap should observe `true` exiting with code 0 within 1s"
        );
    }

    #[tokio::test]
    async fn kill_transitions_to_killed_and_returns_true() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, None);

        assert!(reg.kill(pid));
        assert_eq!(reg.snapshot()[0].status, BgProcessStatus::Killed);

        // Unknown PID → false.
        assert!(!reg.kill(987654));
    }

    #[tokio::test]
    async fn kill_as_caller_enforces_spawner_scope() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, Some(5));

        // Wrong caller(s).
        assert_eq!(reg.kill_as_caller(pid, None), CancelOutcome::Forbidden);
        assert_eq!(reg.kill_as_caller(pid, Some(99)), CancelOutcome::Forbidden);
        assert_eq!(reg.snapshot()[0].status, BgProcessStatus::Running);

        // Correct caller.
        assert_eq!(reg.kill_as_caller(pid, Some(5)), CancelOutcome::Cancelled);
        assert_eq!(reg.snapshot()[0].status, BgProcessStatus::Killed);

        // Unknown PID for any caller → NotFound.
        assert_eq!(reg.kill_as_caller(987654, None), CancelOutcome::NotFound);
    }

    #[tokio::test]
    async fn wait_for_exit_returns_exited_when_child_finishes() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_true_child();
        reg.insert(pid, "true".into(), child, None);

        let outcome = reg
            .wait_for_exit_as_caller(pid, None, Duration::from_secs(2))
            .await;
        assert_eq!(outcome, ProcessWaitOutcome::Exited { code: Some(0) });
    }

    #[tokio::test]
    async fn wait_for_exit_returns_exited_when_already_killed() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, Some(7));
        reg.kill(pid); // status → Killed

        let outcome = reg
            .wait_for_exit_as_caller(pid, Some(7), Duration::from_secs(1))
            .await;
        assert_eq!(outcome, ProcessWaitOutcome::Exited { code: None });
    }

    #[tokio::test]
    async fn wait_for_exit_returns_timed_out_with_snapshot() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, None);

        let outcome = reg
            .wait_for_exit_as_caller(pid, None, Duration::from_millis(150))
            .await;
        match outcome {
            ProcessWaitOutcome::TimedOut(snap) => {
                assert_eq!(snap.pid, pid);
                assert_eq!(snap.status, BgProcessStatus::Running);
                assert_eq!(snap.spawner, None);
            }
            other => panic!("expected TimedOut, got {other:?}"),
        }
        assert_eq!(reg.snapshot().len(), 1, "entry must be preserved on timeout");
    }

    #[tokio::test]
    async fn wait_for_exit_enforces_spawner_scope() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, Some(5));

        assert_eq!(
            reg.wait_for_exit_as_caller(pid, None, Duration::from_millis(20))
                .await,
            ProcessWaitOutcome::Forbidden
        );
        assert_eq!(
            reg.wait_for_exit_as_caller(pid, Some(99), Duration::from_millis(20))
                .await,
            ProcessWaitOutcome::Forbidden
        );
    }

    #[tokio::test]
    async fn wait_for_exit_returns_not_found_for_unknown_pid() {
        let reg = BgRegistry::new();
        assert_eq!(
            reg.wait_for_exit_as_caller(987654, None, Duration::from_millis(10))
                .await,
            ProcessWaitOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn kill_for_spawner_kills_only_matching_running_children() {
        let reg = BgRegistry::new();
        let (p_top, c_top) = spawn_sleep_child();
        let (p_a, c_a) = spawn_sleep_child();
        let (p_b, c_b) = spawn_sleep_child();
        reg.insert(p_top, "top".into(), c_top, None);
        reg.insert(p_a, "a".into(), c_a, Some(7));
        reg.insert(p_b, "b".into(), c_b, Some(9));

        let count = reg.kill_for_spawner(7);
        assert_eq!(count, 1);

        let by_pid: HashMap<u32, BgProcessStatus> =
            reg.snapshot().into_iter().map(|s| (s.pid, s.status)).collect();
        assert_eq!(by_pid[&p_top], BgProcessStatus::Running);
        assert_eq!(by_pid[&p_a], BgProcessStatus::Killed);
        assert_eq!(by_pid[&p_b], BgProcessStatus::Running);

        // Idempotent — second call won't re-kill the now-Killed child.
        assert_eq!(reg.kill_for_spawner(7), 0);
        // Unknown spawner → 0.
        assert_eq!(reg.kill_for_spawner(99), 0);
    }
}
