//! Sandbox violation tracking — ring buffer + pub/sub.
//!
//! Whenever the kernel sandbox denies an operation the failure surfaces
//! in two channels:
//!
//! 1. The command's stderr — `"Operation not permitted"` or similar.
//! 2. The OS audit log — `log stream` on macOS, `dmesg`/journald on Linux.
//!
//! [`SandboxViolationStore`] is the in-process aggregation point for both.
//! Clients (the bash tool, `/sandbox status`, the inference loop) read
//! recent violations from here and present them to the model as
//! `<sandbox_violations>` stderr annotations (CC pattern, #934 §4.7).
//!
//! ## Concurrency
//!
//! Cheap-clone the store and stash it on every [`crate::SandboxRuntime`].
//! Internally a `Mutex<VecDeque>` (writes are rare, on the order of one
//! per failed syscall — coarse-grained locking is fine).
//!
//! ## Why a ring buffer, not unbounded?
//!
//! A buggy or hostile loop could spam thousands of violations per second
//! (`while true; do touch /etc/passwd; done`). Capping at 100 (configurable)
//! bounds memory and avoids drowning the model in noise. Older entries
//! drop silently — clients that need every violation should subscribe.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

/// Maximum violations retained in the ring buffer. Picked to match
/// CC's default; rationale: more than ~100 is unreadable for a human
/// glancing at `/sandbox status` and uninformative for the model.
pub const DEFAULT_RING_CAPACITY: usize = 100;

/// What the kernel actually denied. Open enum because new platforms
/// (Linux landlock, macOS endpoint-security) may surface novel kinds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// Read attempt on a denied path.
    FileRead,
    /// Write/create/unlink attempt on a denied path.
    FileWrite,
    /// Outbound network connection (host:port).
    NetworkOutbound,
    /// Process spawn that the policy doesn't permit.
    ProcessExec,
    /// Anything we can recognize as a sandbox denial but can't classify.
    Other,
}

/// A single sandbox denial event.
///
/// `target` is best-effort — on Linux `bwrap` only emits the destination
/// path for some syscalls; on macOS `sandbox-exec` formats vary by macOS
/// version. Treat the field as an advisory hint, not a precise audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Violation {
    /// What was denied (file read, network out, etc).
    pub kind: ViolationKind,
    /// Path or host:port the operation targeted, when extractable.
    pub target: Option<String>,
    /// The command that triggered the violation, if known. Useful for
    /// correlating stderr noise back to a specific tool invocation.
    pub command: Option<String>,
    /// Wall-clock timestamp. Serialized as RFC 3339 by `serde`.
    #[serde(with = "system_time_serde")]
    pub at: SystemTime,
}

impl Violation {
    /// Convenience: build a violation with `at = SystemTime::now()`.
    #[must_use]
    pub fn new(kind: ViolationKind, target: Option<String>, command: Option<String>) -> Self {
        Self {
            kind,
            target,
            command,
            at: SystemTime::now(),
        }
    }

    /// Format for inclusion in a `<sandbox_violations>` stderr block.
    /// Format mirrors CC's: `<verb> <kind-shape> <target>`.
    #[must_use]
    pub fn render_line(&self) -> String {
        let kind = match self.kind {
            ViolationKind::FileRead => "deny file-read*",
            ViolationKind::FileWrite => "deny file-write*",
            ViolationKind::NetworkOutbound => "deny network-outbound",
            ViolationKind::ProcessExec => "deny process-exec*",
            ViolationKind::Other => "deny",
        };
        match &self.target {
            Some(t) => format!("{kind} {t}"),
            None => kind.to_string(),
        }
    }
}

/// Cheap-clonable handle to a bounded ring of recent [`Violation`]s.
///
/// Designed for low-frequency writes (a few per second at most under
/// real load) and burst reads (one read at the end of every tool call).
/// A `Mutex` is the right primitive here — `RwLock` would only help if
/// reads dramatically outnumbered writes, which they don't.
#[derive(Debug, Clone)]
pub struct SandboxViolationStore {
    inner: Arc<Mutex<VecDeque<Violation>>>,
    capacity: usize,
}

impl SandboxViolationStore {
    /// Create a store with the default ring capacity ([`DEFAULT_RING_CAPACITY`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_RING_CAPACITY)
    }

    /// Create a store with an explicit capacity.
    ///
    /// `capacity = 0` disables retention (useful in tests that want to
    /// exercise the recording path without holding state across calls).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Append a violation to the ring. Drops the oldest entry if full.
    ///
    /// Lock poisoning is intentionally swallowed — losing a violation log
    /// entry is far less bad than panicking the inference loop. We just
    /// proceed without recording.
    pub fn record(&self, v: Violation) {
        if self.capacity == 0 {
            return;
        }
        let Ok(mut buf) = self.inner.lock() else {
            return;
        };
        if buf.len() == self.capacity {
            buf.pop_front();
        }
        buf.push_back(v);
    }

    /// Snapshot of all currently retained violations, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Violation> {
        self.inner
            .lock()
            .map(|buf| buf.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot + clear in a single critical section. Used by the bash
    /// tool to drain violations between commands so we don't double-report.
    #[must_use]
    pub fn drain(&self) -> Vec<Violation> {
        let Ok(mut buf) = self.inner.lock() else {
            return Vec::new();
        };
        buf.drain(..).collect()
    }

    /// Number of violations currently retained (mostly for tests/metrics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// `true` if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Per-kind tally over the current ring contents. Phase 5 of #934
    /// — the headline "violation counts per session" telemetry. Cheap
    /// (one lock + one pass over up to `DEFAULT_RING_CAPACITY` entries)
    /// so callers can read it on every `/sandbox status` invocation
    /// without worrying about overhead.
    ///
    /// Returns counts for the variants present in the buffer; absent
    /// kinds are simply not in the map. Use `unwrap_or(0)` at the
    /// read site if you want a zero default.
    #[must_use]
    pub fn count_by_kind(&self) -> std::collections::HashMap<ViolationKind, usize> {
        let buf = match self.inner.lock() {
            Ok(b) => b,
            Err(_) => return std::collections::HashMap::new(),
        };
        let mut counts = std::collections::HashMap::new();
        for v in buf.iter() {
            *counts.entry(v.kind.clone()).or_insert(0) += 1;
        }
        counts
    }
}

impl Default for SandboxViolationStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a slice of violations as the CC `<sandbox_violations>` stderr
/// block. Returns `None` if there are no violations — callers should not
/// emit an empty block.
#[must_use]
pub fn render_block(violations: &[Violation]) -> Option<String> {
    if violations.is_empty() {
        return None;
    }
    let mut out = String::from("<sandbox_violations>\n");
    for v in violations {
        out.push_str(&v.render_line());
        out.push('\n');
    }
    out.push_str("</sandbox_violations>");
    Some(out)
}

/// Cheap-clone handle to the process-wide violation store.
///
/// All sandboxed tools should record into this single store so
/// `/sandbox status` (and the future Phase 2 telemetry layer) sees
/// every violation across the session, not just the ones that happen
/// to land on the same `SandboxRuntime` instance. The store is created
/// lazily on first access and lives for the rest of the process.
///
/// Per-agent stores are a Phase 2+ concern when slots become real;
/// until then, a single bounded ring is the right primitive.
#[must_use]
pub fn global_store() -> SandboxViolationStore {
    static GLOBAL: OnceLock<SandboxViolationStore> = OnceLock::new();
    GLOBAL.get_or_init(SandboxViolationStore::new).clone()
}

// ── SystemTime <-> RFC 3339 serde helper ────────────────────────────────────

mod system_time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> Result<S::Ok, S::Error> {
        // Seconds since epoch as f64 — survives clock skew and is human-glanceable.
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        secs.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<SystemTime, D::Error> {
        let secs = f64::deserialize(d)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_secs_f64(secs))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_at_capacity() {
        let store = SandboxViolationStore::with_capacity(3);
        for i in 0..5 {
            store.record(Violation::new(
                ViolationKind::FileWrite,
                Some(format!("/tmp/v{i}")),
                None,
            ));
        }
        let snap = store.snapshot();
        assert_eq!(snap.len(), 3);
        // Oldest two dropped → we should see v2, v3, v4.
        assert_eq!(snap[0].target.as_deref(), Some("/tmp/v2"));
        assert_eq!(snap[2].target.as_deref(), Some("/tmp/v4"));
    }

    #[test]
    fn capacity_zero_records_nothing() {
        let store = SandboxViolationStore::with_capacity(0);
        store.record(Violation::new(ViolationKind::Other, None, None));
        assert!(store.is_empty());
    }

    #[test]
    fn drain_empties_the_buffer() {
        let store = SandboxViolationStore::new();
        store.record(Violation::new(ViolationKind::FileRead, None, None));
        store.record(Violation::new(ViolationKind::FileRead, None, None));
        let drained = store.drain();
        assert_eq!(drained.len(), 2);
        assert!(store.is_empty());
    }

    #[test]
    fn render_block_returns_none_when_empty() {
        assert!(render_block(&[]).is_none());
    }

    #[test]
    fn render_block_matches_cc_shape() {
        let v = Violation::new(
            ViolationKind::FileWrite,
            Some("/Users/me/.ssh/id_rsa".into()),
            None,
        );
        let block = render_block(&[v]).unwrap();
        assert!(block.starts_with("<sandbox_violations>\n"));
        assert!(block.contains("deny file-write* /Users/me/.ssh/id_rsa"));
        assert!(block.ends_with("</sandbox_violations>"));
    }

    #[test]
    fn store_is_send_sync() {
        // Compile-time check — the Arc<Mutex<…>> internals must allow
        // sharing across threads, since slots will pass clones around.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SandboxViolationStore>();
    }

    #[test]
    fn violation_serde_roundtrip() {
        let v = Violation::new(
            ViolationKind::NetworkOutbound,
            Some("evil.com:443".into()),
            Some("curl evil.com".into()),
        );
        let json = serde_json::to_string(&v).unwrap();
        let back: Violation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, v.kind);
        assert_eq!(back.target, v.target);
        assert_eq!(back.command, v.command);
    }

    #[test]
    fn store_clones_share_inner_buffer() {
        // Sub-agent slots will clone the store handle — both clones must
        // see the same ring buffer. Same Arc semantics as Persistence::clone.
        let store = SandboxViolationStore::new();
        let twin = store.clone();
        store.record(Violation::new(ViolationKind::Other, None, None));
        assert_eq!(twin.len(), 1);
    }

    #[test]
    fn global_store_returns_same_handle_across_calls() {
        // Two callers must see each other's writes — otherwise tools
        // can't observe violations recorded by other tools in the
        // same process.
        let a = global_store();
        let b = global_store();
        let before = a.len();
        a.record(Violation::new(ViolationKind::Other, None, None));
        assert_eq!(b.len(), before + 1);
        // Drain so this test doesn't leak state into other tests.
        let _ = a.drain();
    }

    #[test]
    fn count_by_kind_tallies_per_variant() {
        // Phase 5 of #934: prove the new per-kind accessor returns
        // accurate tallies and is robust against zero/empty cases.
        let s = SandboxViolationStore::new();
        assert!(
            s.count_by_kind().is_empty(),
            "empty store should yield empty map"
        );

        s.record(Violation::new(ViolationKind::FileRead, None, None));
        s.record(Violation::new(ViolationKind::FileRead, None, None));
        s.record(Violation::new(ViolationKind::FileWrite, None, None));
        s.record(Violation::new(ViolationKind::NetworkOutbound, None, None));

        let counts = s.count_by_kind();
        assert_eq!(counts.get(&ViolationKind::FileRead), Some(&2));
        assert_eq!(counts.get(&ViolationKind::FileWrite), Some(&1));
        assert_eq!(counts.get(&ViolationKind::NetworkOutbound), Some(&1));
        // ProcessExec was never recorded — absent from the map by
        // design (callers `unwrap_or(0)` if they want a zero default).
        assert!(!counts.contains_key(&ViolationKind::ProcessExec));
        assert_eq!(counts.values().sum::<usize>(), s.len());
    }
}
