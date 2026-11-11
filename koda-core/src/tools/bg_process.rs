//! Background process registry.
//!
//! Tracks processes spawned by `Bash { background: true }` so they can be
//! listed, waited on, killed, and cleaned up (SIGTERM) when the session ends
//! or the spawning sub-agent exits (Model E, see #996).
//!
//! An entry is a process group, as an exec session is in Codex: the command
//! Koda started leads it, and whatever that command started with `&` is in
//! it too. The entry is `Running` while anything in the group is alive, so a
//! `server & echo $!` whose shell exits at once is still Koda's, ports and
//! all. A foreground call that leaves processes behind hands its group over
//! with [`BgRegistry::adopt`].
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

use crate::child_agent::CancelOutcome;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::process::Child;

/// Outcome of [`BgRegistry::wait_for_exit_as_caller`].
///
/// Carries process-specific terminal info (exit code) for shell-style
/// background processes. Distinct from agent-completion outcomes
/// (those flow through `ChildAgentRegistry::drain_completed`)
/// because exit-code semantics don't translate cleanly to agent
/// `BgAgentResult` shape and vice-versa.
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
    /// Other PIDs seen in this entry's group (a server its shell started with
    /// `&`), so a check after the group is gone still knows them as Koda's.
    pub members: Vec<u32>,
}

/// Metadata stored alongside the child handle.
struct BgEntry {
    /// The original shell command string.
    command: String,
    /// The spawned child process handle (used for `start_kill` on
    /// kill-paths and `try_wait` on reap). `None` for a group taken over
    /// from a foreground call, whose leader that call already reaped.
    child: Option<Child>,
    /// The leader's exit code, once it has exited; the group may outlive it.
    leader_code: Option<i32>,
    /// When the entry was inserted. Drives `age` in snapshots.
    started_at: Instant,
    /// Current status. Only [`BgRegistry::reap`] and the kill-paths
    /// transition this away from `Running`.
    status: BgProcessStatus,
    /// Sub-agent that spawned this process. `None` = top-level.
    spawner: Option<u32>,
    /// PIDs other than the leader seen in the group ([`BgRegistry::note_member`]).
    members: Vec<u32>,
    /// The socket the command's sandbox serves its network namespace on, so
    /// later commands can reach its servers (bwrap only).
    loopback_socket: Option<std::path::PathBuf>,
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
                child: Some(child),
                leader_code: None,
                started_at: Instant::now(),
                status: BgProcessStatus::Running,
                spawner,
                members: Vec::new(),
                loopback_socket: None,
            },
        );
        pid
    }

    /// Take over the process group a foreground call left running: its
    /// leader (the call's shell) has exited and been reaped, but processes it
    /// started with `&` are still in the group. Keyed by the group's id, the
    /// leader's PID, like every entry.
    pub fn adopt(&self, group: u32, command: String, spawner: Option<u32>) -> u32 {
        self.inner.lock().unwrap().insert(
            group,
            BgEntry {
                command,
                child: None,
                leader_code: None,
                started_at: Instant::now(),
                status: BgProcessStatus::Running,
                spawner,
                members: Vec::new(),
                loopback_socket: None,
            },
        );
        group
    }

    /// Remember that `pid` is in the group of the entry `group`, so it's still
    /// known as Koda's once the group is gone (`kill -0 PID` after a stop).
    pub fn note_member(&self, group: u32, pid: u32) {
        if let Some(entry) = self.inner.lock().unwrap().get_mut(&group)
            && pid != group
            && !entry.members.contains(&pid)
        {
            entry.members.push(pid);
        }
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
        // Reap first, so a process that exited or was killed from outside
        // Koda stops showing as running. The TUI draws its background panel
        // from this every frame, and nothing else reaps on that path.
        self.reap();
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
                members: e.members.clone(),
            })
            .collect();
        out.sort_by_key(|s| s.pid);
        out
    }

    /// Scoped snapshot for the `ListBackgroundTasks` LLM tool. Same
    /// Model E rule as the retired `ChildAgentRegistry::snapshot_for_caller`:
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
        for (group, entry) in guard.iter_mut() {
            if entry.status != BgProcessStatus::Running {
                continue;
            }
            if let Some(child) = entry.child.as_mut()
                && entry.leader_code.is_none()
            {
                match child.try_wait() {
                    Ok(Some(exit)) => entry.leader_code = Some(exit.code().unwrap_or(-1)),
                    Ok(None) => continue, // the leader is still running
                    Err(e) => {
                        // try_wait can fail if the OS lost track (rare).
                        tracing::warn!("BgRegistry reap try_wait failed for PID {group}: {e}");
                        entry.leader_code = Some(-1);
                    }
                }
            }
            // The leader is gone; the group lives on while anything in it does.
            if !koda_sandbox::process_group::group_alive(*group) {
                let code = entry.leader_code.filter(|c| *c >= 0);
                entry.status = BgProcessStatus::Exited { code };
            }
        }
    }

    /// Send SIGTERM to a tracked PID. Returns `true` if the PID was
    /// known (whether or not the kill succeeded — the underlying error
    /// is logged).
    ///
    /// Status flips to `Killed` immediately; the reaper will surface
    /// the eventual `Exited { code }` once the child is reaped by the
    /// kernel. **Unscoped** — the TUI's cancel-all uses it; scoped
    /// callers go through [`Self::kill_as_caller`].
    pub fn kill(&self, pid: u32) -> bool {
        let mut guard = self.inner.lock().unwrap();
        let Some(entry) = guard.get_mut(&pid) else {
            return false;
        };
        if entry.status == BgProcessStatus::Running {
            if let Err(e) = kill_command(pid, entry.child.as_mut()) {
                tracing::warn!("BgRegistry::kill: failed to SIGTERM PID {pid}: {e}");
            }
            entry.status = BgProcessStatus::Killed;
        }
        true
    }

    /// Scoped kill for the `CancelTask` LLM tool. Same Model E rule
    /// as the retired `ChildAgentRegistry::cancel_as_caller`.
    pub fn kill_as_caller(&self, pid: u32, caller_spawner: Option<u32>) -> CancelOutcome {
        let mut guard = self.inner.lock().unwrap();
        let Some(entry) = guard.get_mut(&pid) else {
            return CancelOutcome::NotFound;
        };
        if entry.spawner != caller_spawner {
            return CancelOutcome::Forbidden;
        }
        if entry.status == BgProcessStatus::Running {
            if let Err(e) = kill_command(pid, entry.child.as_mut()) {
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
                    members: entry.members.clone(),
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
        for (group, entry) in guard.iter_mut() {
            if entry.spawner != Some(spawner) {
                continue;
            }
            if entry.status == BgProcessStatus::Running {
                if let Err(e) = kill_command(*group, entry.child.as_mut()) {
                    tracing::warn!("BgRegistry::kill_for_spawner: SIGTERM PID {group}: {e}");
                }
                entry.status = BgProcessStatus::Killed;
                count += 1;
            }
        }
        count
    }
}

/// SIGKILL a background command and whatever it started. `spawn_background`
/// makes each command a process-group leader, so its group holds the
/// command's children too (`npm run dev` and the server it runs). A child
/// that doesn't lead a group has no group of that id, and only it dies.
fn kill_command(group: u32, child: Option<&mut Child>) -> std::io::Result<()> {
    koda_sandbox::process_group::kill_group(group);
    match child {
        // Already exited is fine: the group was the point.
        Some(child) => child.start_kill().or_else(|e| {
            if e.kind() == std::io::ErrorKind::InvalidInput {
                Ok(())
            } else {
                Err(e)
            }
        }),
        None => Ok(()),
    }
}

impl BgRegistry {
    /// Record the socket `pid`'s sandbox serves its network namespace on.
    pub fn set_loopback_socket(&self, pid: u32, socket: std::path::PathBuf) {
        if let Some(entry) = self.inner.lock().unwrap().get_mut(&pid) {
            entry.loopback_socket = Some(socket);
        }
    }

    /// The servers of the running background commands that a sandboxed
    /// command may reach (`koda_sandbox::Loopback`): their ports on macOS,
    /// and on Linux the ports plus the sockets that lead into their network
    /// namespaces.
    pub async fn loopback(&self) -> koda_sandbox::Loopback {
        #[cfg(target_os = "linux")]
        let loopback = koda_sandbox::Loopback {
            forwards: self.loopback_forwards().await,
            ..Default::default()
        };
        #[cfg(not(target_os = "linux"))]
        let loopback = koda_sandbox::Loopback {
            ports: self.lsof_ports().await,
            ..Default::default()
        };
        loopback
    }

    /// The loopback TCP ports the running background commands listen on,
    /// their children's included.
    pub async fn listening_ports(&self) -> Vec<u16> {
        let loopback = self.loopback().await;
        let mut ports = loopback.ports;
        ports.extend(loopback.forwards.iter().map(|f| f.port));
        ports
    }

    /// Ask each running background command's socket for its ports. The
    /// socket of a command that has stopped is removed.
    #[cfg(target_os = "linux")]
    async fn loopback_forwards(&self) -> Vec<koda_sandbox::LoopbackForward> {
        self.reap();
        let mut sockets = Vec::new();
        for entry in self.inner.lock().unwrap().values_mut() {
            if entry.status == BgProcessStatus::Running {
                sockets.extend(entry.loopback_socket.clone());
            } else if let Some(socket) = entry.loopback_socket.take() {
                let _ = std::fs::remove_file(socket);
            }
        }
        if sockets.is_empty() {
            return Vec::new();
        }
        let query = tokio::task::spawn_blocking(move || {
            let mut forwards = Vec::new();
            for socket in sockets {
                match koda_sandbox::bwrap_loopback::listening_ports(&socket) {
                    Ok(ports) => {
                        forwards.extend(ports.into_iter().map(|port| {
                            koda_sandbox::LoopbackForward {
                                port,
                                socket: socket.clone(),
                            }
                        }));
                    }
                    // Not served yet, or its sandbox has gone.
                    Err(e) => tracing::debug!("loopback socket {}: {e}", socket.display()),
                }
            }
            forwards
        });
        let mut forwards = query.await.unwrap_or_default();
        // Two commands can't both listen on one port of a forwarding
        // command's loopback; the first keeps it.
        let mut seen = std::collections::HashSet::new();
        forwards.retain(|f| seen.insert(f.port));
        forwards
    }

    /// Loopback TCP ports the running background commands' process groups
    /// listen on, found with `lsof`: seatbelt shares the host's loopback.
    #[cfg(not(target_os = "linux"))]
    async fn lsof_ports(&self) -> Vec<u16> {
        if !cfg!(target_os = "macos") {
            return Vec::new();
        }
        let groups: Vec<String> = self
            .snapshot()
            .into_iter()
            .filter(|s| s.status == BgProcessStatus::Running)
            .map(|s| s.pid.to_string())
            .collect();
        if groups.is_empty() {
            return Vec::new();
        }
        let lsof = tokio::process::Command::new("lsof")
            .args(["-nP", "-a", "-g", &groups.join(",")])
            .args(["-iTCP", "-sTCP:LISTEN", "-Fn"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(Duration::from_secs(2), lsof).await {
            Ok(Ok(out)) => parse_lsof_ports(&String::from_utf8_lossy(&out.stdout)),
            Ok(Err(e)) => {
                tracing::warn!("BgRegistry::lsof_ports: lsof failed: {e}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!("BgRegistry::lsof_ports: lsof took over 2s");
                Vec::new()
            }
        }
    }
}

/// Ports from `lsof -Fn` output: lines like `n*:8766`, `n127.0.0.1:3000` or
/// `n[::1]:5173`, between the `p` (process) and `f` (file) lines.
#[cfg(any(not(target_os = "linux"), test))]
fn parse_lsof_ports(out: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = out
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter_map(|addr| addr.rsplit_once(':')?.1.parse().ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
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
            if let Err(e) = kill_command(*pid, entry.child.as_mut()) {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[test]
    fn lsof_ports_come_from_the_name_lines() {
        let out = "p4242\nf3\nn*:8766\nf4\nn127.0.0.1:3000\nf5\nn[::1]:5173\nf6\nn*:8766\n";
        assert_eq!(parse_lsof_ports(out), vec![3000, 5173, 8766]);
        assert!(parse_lsof_ports("").is_empty());
    }

    #[tokio::test]
    async fn no_background_command_means_no_ports_and_no_lsof() {
        assert!(BgRegistry::new().listening_ports().await.is_empty());
    }

    /// The TUI's background panel reads `snapshot` and nothing else, so a
    /// process killed outside Koda must drop off it without a separate reap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_sees_an_exited_child_without_a_separate_reap() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_true_child();
        reg.insert(pid, "true".into(), child, None);
        let mut seen = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if reg.snapshot()[0].status != BgProcessStatus::Running {
                seen = true;
                break;
            }
        }
        assert!(seen, "snapshot kept showing an exited child as running");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_transitions_to_killed_and_returns_true() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_sleep_child();
        reg.insert(pid, "sleep 60".into(), child, None);

        assert!(reg.kill(pid));
        assert_eq!(reg.snapshot()[0].status, BgProcessStatus::Killed);

        // Unknown PID → false.
        assert!(!reg.kill(987654));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_exit_returns_exited_when_child_finishes() {
        let reg = BgRegistry::new();
        let (pid, child) = spawn_true_child();
        reg.insert(pid, "true".into(), child, None);

        let outcome = reg
            .wait_for_exit_as_caller(pid, None, Duration::from_secs(2))
            .await;
        assert_eq!(outcome, ProcessWaitOutcome::Exited { code: Some(0) });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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
        assert_eq!(
            reg.snapshot().len(),
            1,
            "entry must be preserved on timeout"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_exit_returns_not_found_for_unknown_pid() {
        let reg = BgRegistry::new();
        assert_eq!(
            reg.wait_for_exit_as_caller(987654, None, Duration::from_millis(10))
                .await,
            ProcessWaitOutcome::NotFound
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

        let by_pid: HashMap<u32, BgProcessStatus> = reg
            .snapshot()
            .into_iter()
            .map(|s| (s.pid, s.status))
            .collect();
        assert_eq!(by_pid[&p_top], BgProcessStatus::Running);
        assert_eq!(by_pid[&p_a], BgProcessStatus::Killed);
        assert_eq!(by_pid[&p_b], BgProcessStatus::Running);

        // Idempotent — second call won't re-kill the now-Killed child.
        assert_eq!(reg.kill_for_spawner(7), 0);
        // Unknown spawner → 0.
        assert_eq!(reg.kill_for_spawner(99), 0);
    }
}
