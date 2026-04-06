//! Background process registry.
//!
//! Tracks processes spawned by `Bash { background: true }` so they can be
//! listed, and so they are cleaned up (SIGTERM) when the session ends.
//!
//! ## Usage
//!
//! ```text
//! Model calls: Bash { command: "npm run dev", background: true }
//!   → Process spawned, PID recorded in BgRegistry
//!   → Tool returns immediately: "Started PID 12345"
//!   → Model continues with other work
//!   → On session end: all tracked PIDs receive SIGTERM
//! ```
//!
//! ## Design
//!
//! Each `ToolRegistry` owns one `BgRegistry`. All background processes for
//! the session are keyed by PID. The registry is `Mutex`-protected since
//! the spawning thread and the cleanup path may differ.

use std::collections::HashMap;
use std::sync::Mutex;
use tokio::process::Child;

/// Metadata stored alongside the child handle.
pub struct BgEntry {
    /// The original shell command string.
    pub command: String,
    /// The spawned child process handle (used for `start_kill` on drop).
    pub child: Child,
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

    /// Register a spawned child.  Returns the PID.
    pub fn insert(&self, pid: u32, command: String, child: Child) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .insert(pid, BgEntry { command, child });
        pid
    }

    /// Return a snapshot of running PIDs + commands for display.
    pub fn list(&self) -> Vec<(u32, String)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(pid, e)| (*pid, e.command.clone()))
            .collect()
    }

    /// How many processes are currently tracked.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Returns `true` if no background processes are tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

impl Default for BgRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BgRegistry {
    /// Best-effort SIGTERM all tracked processes when the session ends.
    fn drop(&mut self) {
        let mut guard = self.inner.lock().unwrap();
        for (pid, entry) in guard.iter_mut() {
            // start_kill is sync (sends SIGTERM on Unix / TerminateProcess on Windows).
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

    #[test]
    fn registry_starts_empty() {
        let reg = BgRegistry::new();
        assert_eq!(reg.len(), 0);
        assert!(reg.list().is_empty());
    }

    #[test]
    fn is_empty_reflects_state() {
        let reg = BgRegistry::new();
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn insert_increments_len_and_appears_in_list() {
        let reg = BgRegistry::new();
        // Spawn a trivial command so we have a real Child handle.
        let child = tokio::process::Command::new("true").spawn().unwrap();
        let pid = child.id().unwrap_or(9999);
        let returned_pid = reg.insert(pid, "true".into(), child);
        assert_eq!(returned_pid, pid);
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        let entries = reg.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, pid);
        assert_eq!(entries[0].1, "true");
    }
}
