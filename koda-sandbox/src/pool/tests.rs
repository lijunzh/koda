//! Pool + slot tests. Split out of `pool.rs` to keep that file under
//! 600 LOC and to let test helpers live without polluting the public
//! surface.
//!
//! ## Worker spawning in tests
//!
//! Tests need `KODA_FS_WORKER_BIN` to point at a real `koda-fs-worker`
//! binary. Cargo sets `CARGO_BIN_EXE_koda-fs-worker` for integration
//! tests in the same workspace; unit tests in this module
//! (`#[cfg(test)]`) get it via the `[[bin]]` declaration in
//! `koda-sandbox/Cargo.toml`. If you see "koda-fs-worker not found"
//! while developing, run `cargo build -p koda-fs-worker` first or
//! set `KODA_FS_WORKER_BIN` to its target/debug/ path.

use super::*;

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;
use tokio::sync::Notify;

use crate::ipc::Request;
use crate::policy::SandboxPolicy;
use crate::workspace::{CwdProvider, WorkspaceProvider};

// ── Mock provider ────────────────────────────────────────────────

/// Counts provision/release calls so tests can assert lifecycle
/// behavior without a real filesystem provider in the loop.
///
/// `release_notify` fires inside `release()` *after* the counter is
/// incremented, so tests that need to know "the spawned release task
/// actually ran" can `notify.notified().await` with a deadline
/// instead of guessing a settle time. See #1312 for the rationale
/// ("test the decision, not the side effect").
#[derive(Debug, Default)]
struct CountingProvider {
    project_root: PathBuf,
    provisioned: AtomicUsize,
    released: AtomicUsize,
    release_notify: Notify,
}

impl CountingProvider {
    fn new(project_root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            project_root,
            provisioned: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
            release_notify: Notify::new(),
        })
    }
}

#[async_trait]
impl WorkspaceProvider for CountingProvider {
    async fn provision(&self, _slot_id: &str) -> Result<PathBuf> {
        self.provisioned.fetch_add(1, Ordering::SeqCst);
        Ok(self.project_root.clone())
    }
    async fn release(&self, _slot_id: &str, _path: &Path) -> Result<Option<String>> {
        self.released.fetch_add(1, Ordering::SeqCst);
        // Notify *after* the counter is bumped so any awaiter that
        // wakes immediately sees the post-increment value. `notify_one`
        // is buffered — even if no awaiter is parked yet, the next
        // `notified()` call returns immediately. (See `Notify` docs:
        // "if no permits are available, this method awaits the next call
        // to notify_one". The flip side: `notify_one` issued before any
        // `notified()` is recorded as a permit and consumed by the next
        // call.)
        self.release_notify.notify_one();
        Ok(None)
    }
}

/// Provider whose `provision` always errors. Used to prove that an
/// acquire failure after worker checkout doesn't leak the worker
/// back into the pool — the worker must be killed because we have
/// no way to know if it's still in a clean state.
#[derive(Debug)]
struct FailingProvider;

#[async_trait]
impl WorkspaceProvider for FailingProvider {
    async fn provision(&self, _slot_id: &str) -> Result<PathBuf> {
        anyhow::bail!("synthetic provision failure for test")
    }
    async fn release(&self, _slot_id: &str, _path: &Path) -> Result<Option<String>> {
        Ok(None)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Make sure the worker binary path is wired up for these tests.
/// Cargo sets `CARGO_BIN_EXE_koda-fs-worker` for integration tests
/// but not for `#[cfg(test)]` modules in the lib crate, so we shell
/// out to find it the same way `worker_client::worker_binary()` does
/// for production.
///
/// **#1109 F1**: was `unsafe { std::env::set_var(...) }` (UB in
/// Rust 2024 if any other thread reads env). Now uses
/// [`crate::worker_client::set_worker_binary_for_tests`] — a
/// `OnceLock`-backed override that's safe under concurrent test
/// execution.
fn ensure_worker_bin() {
    if std::env::var("KODA_FS_WORKER_BIN").is_ok() {
        return;
    }
    if std::env::var("CARGO_BIN_EXE_koda-fs-worker").is_ok() {
        return;
    }
    // Fall back to the workspace target dir: tests under `cargo test
    // -p koda-sandbox` rely on `cargo build` having produced this.
    let mut p = std::env::current_exe().expect("current_exe");
    while p.pop() {
        if p.ends_with("debug") || p.ends_with("release") {
            let bin = p.join("koda-fs-worker");
            if bin.exists() {
                crate::worker_client::set_worker_binary_for_tests(bin);
                return;
            }
        }
    }
}

fn pool_dir() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

// ── Construction & basic invariants ───────────────────────────────────────

#[test]
#[should_panic(expected = "target_per_bucket must be >= 1")]
fn new_with_zero_capacity_panics() {
    // Zero-capacity buckets would mean every return-to-pool drops the
    // worker, defeating the entire purpose of the pool. Better to
    // catch the misconfiguration loudly at construction than to
    // silently degrade to fork-per-acquire forever.
    let _ = SandboxPool::new(0);
}

#[test]
fn new_pool_is_empty() {
    let pool = SandboxPool::new(4);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.bucket_count(), 0);
}

// ── Acquire & return ──────────────────────────────────────────────────────

#[tokio::test]
async fn cold_acquire_spawns_worker_and_calls_provision() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    let slot = pool
        .acquire(
            provider.clone(),
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "slot-cold-1".to_string(),
        )
        .await
        .expect("acquire");

    assert_eq!(provider.provisioned.load(Ordering::SeqCst), 1);
    assert_eq!(slot.slot_id(), "slot-cold-1");
    assert_eq!(slot.workspace_path(), dir.path());
    // No worker in the pool yet — it's checked out.
    assert_eq!(pool.idle_count(), 0);
}

#[tokio::test]
async fn drop_returns_clean_worker_to_pool() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    {
        let _slot = pool
            .acquire(
                provider.clone(),
                dir.path().to_path_buf(),
                &SandboxPolicy::default(),
                None,
                "slot-return-1".to_string(),
            )
            .await
            .expect("acquire");
    }
    // Drop is sync — the worker is back in the pool by the time
    // control returns here.
    assert_eq!(pool.idle_count(), 1, "clean drop must return worker");
}

#[tokio::test]
async fn warm_then_acquire_uses_warm_worker() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    pool.warm_bucket(dir.path().to_path_buf(), &SandboxPolicy::default(), None, 2)
        .await
        .expect("warm");
    assert_eq!(pool.idle_count(), 2);

    let _slot = pool
        .acquire(
            provider,
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "slot-warm-1".to_string(),
        )
        .await
        .expect("acquire");

    // One worker should have moved out of the free-list into the
    // slot; the other stays warm for the next acquire.
    assert_eq!(
        pool.idle_count(),
        1,
        "acquire must consume from warm bucket"
    );
}

#[tokio::test]
async fn warm_bucket_respects_target_cap() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2); // cap = 2
    pool.warm_bucket(dir.path().to_path_buf(), &SandboxPolicy::default(), None, 5)
        .await
        .expect("warm");
    assert_eq!(
        pool.idle_count(),
        2,
        "warm_bucket must cap at target_per_bucket regardless of requested n"
    );
}

#[tokio::test]
async fn warm_bucket_is_idempotent_at_cap() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    pool.warm_bucket(dir.path().to_path_buf(), &SandboxPolicy::default(), None, 2)
        .await
        .expect("warm 1");
    pool.warm_bucket(dir.path().to_path_buf(), &SandboxPolicy::default(), None, 2)
        .await
        .expect("warm 2");
    assert_eq!(
        pool.idle_count(),
        2,
        "second warm must NOT spawn extras when bucket is already at cap"
    );
}

#[tokio::test]
async fn return_to_full_bucket_drops_worker() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(1); // tight cap
    let provider = CountingProvider::new(dir.path().to_path_buf());

    // Two slots out at once → cap=1, so the second drop has nowhere
    // to land and the worker must be killed instead of stashed.
    let s1 = pool
        .acquire(
            provider.clone(),
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "s1".to_string(),
        )
        .await
        .expect("a1");
    let s2 = pool
        .acquire(
            provider.clone(),
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "s2".to_string(),
        )
        .await
        .expect("a2");

    drop(s1);
    assert_eq!(pool.idle_count(), 1, "first drop fills the cap");
    drop(s2);
    assert_eq!(
        pool.idle_count(),
        1,
        "second drop must be discarded because bucket is at cap"
    );
}

#[tokio::test]
async fn dirty_slot_does_not_return_worker_to_pool() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    {
        let mut slot = pool
            .acquire(
                provider.clone(),
                dir.path().to_path_buf(),
                &SandboxPolicy::default(),
                None,
                "dirty".to_string(),
            )
            .await
            .expect("acquire");
        // Simulate "the worker errored mid-RPC and might be in
        // inconsistent state" — caller flags it as not-reusable.
        slot.mark_dirty();
    }
    assert_eq!(
        pool.idle_count(),
        0,
        "dirty drop must kill the worker, not return it"
    );
}

#[tokio::test]
async fn distinct_policies_do_not_share_bucket() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    // The security invariant: a worker spawned with policy A must
    // NEVER serve a request that asked for policy B. The pool
    // enforces this via the bucket key including the full policy.
    let policy_a = SandboxPolicy::default();
    let mut policy_b = SandboxPolicy::default();
    policy_b.fs.allow_git_config = true; // any field difference works

    {
        let _ = pool
            .acquire(
                provider.clone(),
                dir.path().to_path_buf(),
                &policy_a,
                None,
                "pa".to_string(),
            )
            .await
            .expect("a");
    }
    {
        let _ = pool
            .acquire(
                provider.clone(),
                dir.path().to_path_buf(),
                &policy_b,
                None,
                "pb".to_string(),
            )
            .await
            .expect("b");
    }
    assert_eq!(
        pool.bucket_count(),
        2,
        "policies that differ in any field must land in distinct buckets"
    );
}

// ── Failure modes ─────────────────────────────────────────────────────────

#[tokio::test]
async fn provision_failure_does_not_leak_worker_into_pool() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let bad_provider: Arc<dyn WorkspaceProvider> = Arc::new(FailingProvider);

    let res = pool
        .acquire(
            bad_provider,
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "leaky".to_string(),
        )
        .await;
    assert!(res.is_err(), "acquire must propagate the provision error");
    assert_eq!(
        pool.idle_count(),
        0,
        "failed acquire must NOT return the half-built worker to the pool \
         (we can't prove its state, so kill it)"
    );
}

// ── Pool drop ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn pool_drop_releases_warm_workers() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(3);
    pool.warm_bucket(dir.path().to_path_buf(), &SandboxPolicy::default(), None, 3)
        .await
        .expect("warm");
    assert_eq!(pool.idle_count(), 3);

    // Capture the strong count to prove pool drop happens here.
    let weak = Arc::downgrade(&pool);
    drop(pool);
    assert!(
        weak.upgrade().is_none(),
        "pool drop must release its Arc storage so the SandboxPool struct \
         is destructed (which kills every cached worker via its Drop)"
    );
}

#[tokio::test]
async fn slot_outliving_pool_still_drops_cleanly() {
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = CountingProvider::new(dir.path().to_path_buf());

    let slot = pool
        .acquire(
            provider.clone(),
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "orphan".to_string(),
        )
        .await
        .expect("acquire");

    // Pool gone before slot.
    drop(pool);

    // Park a `notified()` listener BEFORE we drop the slot. `Notify`
    // is buffered (`notify_one` issued without a parked awaiter is
    // recorded as one permit), so even if the spawned release task
    // races ahead and signals before we await, the await returns
    // immediately by consuming the permit. Listener-first ordering
    // closes the (theoretical) window between `drop(slot)` and the
    // listener registration.
    let release_signal = provider.release_notify.notified();
    tokio::pin!(release_signal);

    // This drop must NOT panic — the slot uses Weak<Pool> precisely
    // so the worker can be killed (not returned) when the pool is
    // already dead.
    drop(slot);

    // Deterministic readiness (#1312 was: blind `sleep(50ms)`).
    // The release task signals via `release_notify` after
    // incrementing `released`, so by the time `notified()` resolves
    // we know the spawned task has run — not just "50ms have passed."
    tokio::time::timeout(std::time::Duration::from_secs(2), release_signal)
        .await
        .expect(
            "spawned release task did not run within 2s of dropping the orphaned slot \
             — SandboxSlot::Drop's `Handle::try_current() + spawn(release)` regressed",
        );
    assert!(
        provider.released.load(Ordering::SeqCst) >= 1,
        "release should have been spawned even with pool gone"
    );
}

// ── Worker actually works ─────────────────────────────────────────────────

#[tokio::test]
async fn acquired_worker_responds_to_ping() {
    // Belt-and-suspenders: the slot abstraction is worthless if it
    // hands you a busted worker. Round-trip a Ping through the
    // worker we got from acquire.
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = Arc::new(CwdProvider::new(dir.path()));

    let mut slot = pool
        .acquire(
            provider,
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "ping".to_string(),
        )
        .await
        .expect("acquire");

    let resp = slot
        .worker()
        .request(&Request::Ping)
        .await
        .expect("ping ok");
    assert!(matches!(resp, crate::ipc::Response::Pong));
}

#[tokio::test]
async fn warm_acquire_is_faster_than_cold() {
    // The whole reason the pool exists. A warm acquire should beat
    // a cold one by the spawn cost (~50-200ms typical). We use a
    // generous threshold (warm < cold / 2) to avoid flakiness on
    // slow CI runners — the actual ratio is usually >5x.
    ensure_worker_bin();
    let dir = pool_dir();
    let pool = SandboxPool::new(2);
    let provider = Arc::new(CwdProvider::new(dir.path()));

    // Cold: time the spawn.
    let t_cold_start = std::time::Instant::now();
    let _cold = pool
        .acquire(
            provider.clone(),
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "cold".to_string(),
        )
        .await
        .expect("cold");
    let cold = t_cold_start.elapsed();
    drop(_cold); // returns worker to pool

    // Warm: now the bucket has 1 worker.
    let t_warm_start = std::time::Instant::now();
    let _warm = pool
        .acquire(
            provider,
            dir.path().to_path_buf(),
            &SandboxPolicy::default(),
            None,
            "warm".to_string(),
        )
        .await
        .expect("warm");
    let warm = t_warm_start.elapsed();

    // Liberal bound to keep CI green; tighter checks live in benches.
    assert!(
        warm < cold / 2,
        "warm acquire ({warm:?}) must beat cold ({cold:?}) by at least 2x; \
         pool isn't actually amortizing spawn cost"
    );
}
