//! Sandbox pool + slot lifecycle (Phases 4b + 4c of #934).
//!
//! ## What this gives you
//!
//! [`SandboxPool`] is a per-process LRU-free pool of pre-warmed
//! [`WorkerClient`]s keyed by `(writable_root, policy, proxy_port)`.
//! Calling [`SandboxPool::acquire`] returns a [`SandboxSlot`] that
//! owns one worker plus one provisioned workspace. Drop-ing the slot
//! returns the worker to the pool (if still healthy) and asynchronously
//! releases the workspace.
//!
//! ## Why the pool exists
//!
//! Every sandboxed FS / Bash invocation today spawns a fresh
//! `koda-fs-worker`: fork + exec + binary startup + readiness
//! handshake = ~60-250 ms wall clock on a warm laptop. Phase 4 wants
//! `acquire_slot() < 15 ms p95 warm`, which means we have to amortize
//! the spawn cost across multiple acquires of the same `(root, policy)`
//! tuple — exactly the pattern the sub-agent dispatcher hits when it
//! fans out 30 parallel workers against the same project.
//!
//! ## Bucketing
//!
//! Workers bind their `writable_root` and `SandboxPolicy` at spawn
//! time (via `--root` CLI arg + `KODA_FS_WORKER_POLICY` env var). They
//! **cannot** be re-bound after the fact without a worker-protocol
//! change, so the pool buckets workers by the full
//! `(writable_root, policy, proxy)` tuple. A worker spawned with
//! policy A is never reused for a request that asked for policy B —
//! that would be a privilege-escalation bug, not just a perf miss.
//!
//! The bucket key uses **owned** `SandboxPolicy` (not a hash) for the
//! same security reason as [`crate::seatbelt_cache`]: a hash-only key
//! could theoretically let two policies share a free-list entry on
//! collision, silently widening one of them.
//!
//! ## Pre-warming model
//!
//! Pre-warming is **opt-in** via [`SandboxPool::warm_bucket`]. The
//! constructor doesn't pre-warm because it doesn't yet know which
//! buckets will be hit; the dispatcher does, and it can call
//! `warm_bucket` before fanning out work. Cold acquires still work —
//! they just spawn a fresh worker — so callers that don't pre-warm
//! get correctness without any speedup.
//!
//! There is no background fill task. YAGNI for the first cut. Warm
//! buckets stay warm because finished slots return their workers,
//! and the steady-state population matches the working-set size.
//!
//! ## Drop ordering
//!
//! - `SandboxSlot::drop` returns the worker (if clean) and spawns an
//!   async task to release the workspace. The release runs
//!   best-effort: if the tokio runtime is gone, we log and skip.
//! - `SandboxPool::drop` clears all free-list buckets, which drops
//!   every cached `WorkerClient`, which kills the child processes via
//!   `WorkerClient::Drop`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use tracing::{debug, warn};

use crate::policy::SandboxPolicy;
use crate::proxy::ProxyHandle;
use crate::worker_client::WorkerClient;
use crate::workspace::WorkspaceProvider;

// ── Bucket key ────────────────────────────────────────────────────────────

/// Identifies a free-list bucket. Two acquires with byte-identical
/// keys can share a worker; otherwise they cannot.
///
/// The full `SandboxPolicy` is stored (not a hash) so collisions are
/// physically impossible — a hash-only key would be a security defect
/// (see module docs).
///
/// `proxy_port` is just the loopback port number, not a `ProxyHandle`,
/// because two `ProxyHandle`s pointing at the same port are
/// semantically interchangeable from the worker's perspective.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BucketKey {
    writable_root: PathBuf,
    policy: SandboxPolicy,
    proxy_port: Option<u16>,
}

// ── Pool ──────────────────────────────────────────────────────────────────

/// Process-wide pool of pre-warmed `WorkerClient`s.
///
/// Hand-roll your own per-test or share `Arc<SandboxPool>` across the
/// whole koda process — both work. The internal mutex guards a small
/// `HashMap<BucketKey, Vec<WorkerClient>>` so contention is minimal.
pub struct SandboxPool {
    target_per_bucket: usize,
    free: Mutex<HashMap<BucketKey, Vec<WorkerClient>>>,
}

impl std::fmt::Debug for SandboxPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.free.lock().map(|m| m.len()).unwrap_or(0);
        f.debug_struct("SandboxPool")
            .field("target_per_bucket", &self.target_per_bucket)
            .field("buckets", &n)
            .finish()
    }
}

impl SandboxPool {
    /// Construct an empty pool. `target_per_bucket` caps how many
    /// idle workers any single `(root, policy, proxy)` bucket can
    /// hold; surplus returns are dropped (= killed).
    ///
    /// Reasonable values: 1 for sequential-only flows, 4-8 for the
    /// usual desktop case, 30+ for the sub-agent dispatcher's
    /// fan-out demo.
    pub fn new(target_per_bucket: usize) -> Arc<Self> {
        assert!(target_per_bucket > 0, "target_per_bucket must be >= 1");
        Arc::new(Self {
            target_per_bucket,
            free: Mutex::new(HashMap::new()),
        })
    }

    /// Eagerly spawn `n` workers into the bucket for
    /// `(writable_root, policy, proxy)`. Subsequent
    /// [`acquire`](Self::acquire) calls with the same triple will
    /// pop from this pre-warmed set.
    ///
    /// Caps the bucket at `target_per_bucket`: requesting more is a
    /// no-op past the cap, not an error. Returning fewer than `n`
    /// (because the cap was hit) is intentional — the cap exists
    /// to bound resource usage.
    ///
    /// Spawns happen sequentially, not in parallel, because the
    /// fork+exec syscall already has internal serialization on most
    /// kernels and parallel spawning gives nearly zero speedup while
    /// adding error-handling complexity. If this ever shows up on a
    /// flame graph, switch to `futures::future::try_join_all`.
    pub async fn warm_bucket(
        self: &Arc<Self>,
        writable_root: PathBuf,
        policy: &SandboxPolicy,
        proxy: Option<&ProxyHandle>,
        n: usize,
    ) -> Result<()> {
        let key = BucketKey {
            writable_root: writable_root.clone(),
            policy: policy.clone(),
            proxy_port: proxy.map(|p| p.port),
        };

        let already = {
            let map = self.free.lock().expect("pool mutex poisoned");
            map.get(&key).map(|v| v.len()).unwrap_or(0)
        };
        let want = n.min(self.target_per_bucket).saturating_sub(already);
        debug!(
            "warm_bucket: target={n}, cap={}, already={already}, spawning={want}",
            self.target_per_bucket
        );

        for _ in 0..want {
            let client =
                WorkerClient::spawn_with_policy_and_proxy(writable_root.clone(), policy, proxy)
                    .await?;
            let mut map = self.free.lock().expect("pool mutex poisoned");
            // Re-check the cap inside the lock — concurrent warm /
            // return calls might have filled the bucket since we
            // computed `want`. Drop the surplus instead of leaking.
            let bucket = map.entry(key.clone()).or_default();
            if bucket.len() < self.target_per_bucket {
                bucket.push(client);
            } else {
                drop(client); // explicit: kills the worker via WorkerClient::Drop
            }
        }
        Ok(())
    }

    /// Acquire a slot: pop a warm worker for this bucket if one
    /// exists, otherwise spawn a fresh one. Provisions the workspace
    /// via `provider` and bundles everything into a [`SandboxSlot`].
    ///
    /// `proxy` is by-reference because `ProxyHandle` is non-`Clone`;
    /// the slot only needs the port number to identify its bucket.
    ///
    /// On error after worker checkout but before slot construction
    /// (e.g. workspace provisioning fails), the worker is **dropped**
    /// rather than returned to the pool — we have no way to know if
    /// the worker is still in a clean state, and the safe default is
    /// to kill it.
    pub async fn acquire(
        self: &Arc<Self>,
        provider: Arc<dyn WorkspaceProvider>,
        writable_root: PathBuf,
        policy: &SandboxPolicy,
        proxy: Option<&ProxyHandle>,
        slot_id: String,
    ) -> Result<SandboxSlot> {
        let key = BucketKey {
            writable_root: writable_root.clone(),
            policy: policy.clone(),
            proxy_port: proxy.map(|p| p.port),
        };

        // Try the warm path first.
        let warm = {
            let mut map = self.free.lock().expect("pool mutex poisoned");
            map.get_mut(&key).and_then(|bucket| bucket.pop())
        };
        let worker = if let Some(w) = warm {
            debug!("acquire: warm hit for slot_id={slot_id}");
            w
        } else {
            debug!("acquire: cold spawn for slot_id={slot_id}");
            WorkerClient::spawn_with_policy_and_proxy(writable_root, policy, proxy).await?
        };

        // Provision the workspace AFTER we have a worker so a
        // provisioning failure doesn't leak a freshly-spawned worker
        // back to the caller as a half-built slot.
        let workspace_path = match provider.provision(&slot_id).await {
            Ok(p) => p,
            Err(e) => {
                drop(worker); // explicit: don't return half-built worker to pool
                return Err(e);
            }
        };

        Ok(SandboxSlot {
            worker: Some(worker),
            pool: Arc::downgrade(self),
            key,
            provider: Some(provider),
            slot_id,
            workspace_path,
            dirty: false,
        })
    }

    /// Total number of idle workers across all buckets. Test/diag
    /// helper; production callers don't need this.
    pub fn idle_count(&self) -> usize {
        self.free
            .lock()
            .expect("pool mutex poisoned")
            .values()
            .map(Vec::len)
            .sum()
    }

    /// Number of distinct buckets currently tracked. Test/diag
    /// helper; production callers don't need this.
    pub fn bucket_count(&self) -> usize {
        self.free.lock().expect("pool mutex poisoned").len()
    }

    /// Internal: try to return a worker to its bucket. Called from
    /// `SandboxSlot::Drop`. Drops the worker (= kills) if the bucket
    /// is at the cap.
    fn return_worker(&self, key: BucketKey, worker: WorkerClient) {
        let mut map = self.free.lock().expect("pool mutex poisoned");
        let bucket = map.entry(key).or_default();
        if bucket.len() < self.target_per_bucket {
            bucket.push(worker);
        } else {
            // Surplus: dropping kills the worker.
            drop(worker);
        }
    }
}

impl Drop for SandboxPool {
    fn drop(&mut self) {
        // Explicit clear so the order of effects is obvious in
        // logs / valgrind output: every cached WorkerClient is
        // dropped here, which kills its child process.
        if let Ok(mut map) = self.free.lock() {
            map.clear();
        }
    }
}

// ── Slot ──────────────────────────────────────────────────────────────────

/// A single in-flight sandbox session: one worker + one workspace.
///
/// Move-only (no `Clone`): the slot owns mutable access to its
/// worker and the right to release its workspace exactly once.
///
/// ## Drop semantics
///
/// On `drop`:
/// 1. If the slot is **clean** (no `mark_dirty` called) and the pool
///    is still alive, the worker is returned to its bucket (capped
///    at `target_per_bucket`).
/// 2. If the slot is **dirty** OR the pool is gone, the worker is
///    dropped, which kills the child process.
/// 3. The workspace release runs via `tokio::spawn` — best-effort,
///    requires a live tokio runtime. If the runtime is gone we log
///    a warning and skip; the next process restart will GC any
///    leftover state via the provider's own conventions.
///
/// Callers that need synchronous, guaranteed workspace cleanup
/// should call [`SandboxSlot::close`] explicitly before drop. The
/// `Drop` path is a safety net, not the primary release mechanism
/// for production code.
pub struct SandboxSlot {
    /// `Option` because `Drop` needs to move the worker out. Always
    /// `Some` between `acquire` and `drop`.
    worker: Option<WorkerClient>,
    pool: Weak<SandboxPool>,
    key: BucketKey,
    /// Held as `Option` for the same reason as `worker`: `Drop`
    /// moves it out so the async release task can `await` on it.
    provider: Option<Arc<dyn WorkspaceProvider>>,
    slot_id: String,
    workspace_path: PathBuf,
    /// Set to `true` to force this worker to be killed on drop
    /// instead of returned to the pool. Use when an RPC errored or
    /// the worker is in any uncertain state.
    dirty: bool,
}

impl std::fmt::Debug for SandboxSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxSlot")
            .field("slot_id", &self.slot_id)
            .field("workspace_path", &self.workspace_path)
            .field("dirty", &self.dirty)
            .finish()
    }
}

impl SandboxSlot {
    /// Mutable access to the wrapped worker. `expect` is safe
    /// because `worker` is only `None` after `Drop` (which consumes
    /// `self`).
    pub fn worker(&mut self) -> &mut WorkerClient {
        self.worker
            .as_mut()
            .expect("slot worker accessed after drop")
    }

    /// The path the slot should treat as its writable root. Returned
    /// by the workspace provider during `acquire`.
    pub fn workspace_path(&self) -> &std::path::Path {
        &self.workspace_path
    }

    /// The slot's unique id, as supplied to `acquire`. Useful for
    /// log correlation.
    pub fn slot_id(&self) -> &str {
        &self.slot_id
    }

    /// Mark the worker as not safe to reuse. Call this if the worker
    /// returned a transport error or the slot otherwise observed
    /// inconsistent state — the worker will be killed on drop
    /// instead of returned to the pool.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Async release path. Returns the worker to the pool (if clean)
    /// and synchronously awaits the workspace release. Use this when
    /// you need deterministic cleanup ordering.
    ///
    /// Consumes `self` so the `Drop` impl runs without trying to
    /// re-release.
    pub async fn close(mut self) -> Result<()> {
        let worker = self.worker.take().expect("worker present in close()");
        let provider = self.provider.take().expect("provider present in close()");
        if !self.dirty {
            if let Some(pool) = self.pool.upgrade() {
                pool.return_worker(
                    std::mem::replace(
                        &mut self.key,
                        BucketKey {
                            writable_root: PathBuf::new(),
                            policy: SandboxPolicy::default(),
                            proxy_port: None,
                        },
                    ),
                    worker,
                );
            } else {
                // Pool gone — worker dies here.
                drop(worker);
            }
        } else {
            drop(worker);
        }
        // Synchronous await — caller chose this path explicitly.
        match provider.release(&self.slot_id, &self.workspace_path).await {
            Ok(Some(hint)) => {
                debug!("slot {} released with hint: {hint}", self.slot_id);
            }
            Ok(None) => {}
            Err(e) => {
                warn!("slot {} release failed: {e}", self.slot_id);
                return Err(e);
            }
        }
        Ok(())
    }
}

impl Drop for SandboxSlot {
    fn drop(&mut self) {
        // 1. Worker disposition.
        if let Some(worker) = self.worker.take() {
            if self.dirty {
                drop(worker);
            } else if let Some(pool) = self.pool.upgrade() {
                pool.return_worker(self.key.clone(), worker);
            } else {
                drop(worker);
            }
        }

        // 2. Workspace release: best-effort async.
        if let Some(provider) = self.provider.take() {
            let slot_id = std::mem::take(&mut self.slot_id);
            let workspace_path = std::mem::take(&mut self.workspace_path);
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        if let Err(e) = provider.release(&slot_id, &workspace_path).await {
                            warn!("slot {slot_id} async release failed: {e}");
                        }
                    });
                }
                Err(_) => {
                    warn!(
                        "slot {slot_id} dropped outside tokio runtime; \
                         workspace release skipped — call close() for \
                         deterministic cleanup"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
