//! Phase 4g of #934: prove `SandboxPool` hits its acceptance gate.
//!
//! Two benches in one binary because they share the worker-binary
//! discovery + pool-construction setup:
//!
//! - **`acquire_slot_warm`** — the explicit gate. Pre-warms the pool
//!   then times `pool.acquire()` over 1000 iterations. Exits non-zero
//!   if **p95 > 15 ms** (the bar `#934 §12 / §9` sets).
//! - **`acquire_slot_cold`** — worst-case data. Empty pool, every
//!   acquire forces a fresh worker spawn. No pass/fail; this exists
//!   so we can quote a real cold-start number when explaining why
//!   the warm bench matters.
//!
//! Output:
//!
//! - Human-readable summary to stdout.
//! - One `BENCH_JSON: {...}` line per bench at the end, structured
//!   for `bench-results/phase-4g/` archival and regression diffing.
//!
//! Run via `cargo bench --bench acquire_slot -p koda-sandbox`.
//! Cargo's bench runner builds in release mode by default, so the
//! numbers are representative of what users actually see.
//!
//! ## Why no criterion
//!
//! koda's existing bench (`koda-cli/benches/render_bench.rs`) is
//! handwritten with `Instant`. Following that convention keeps the
//! dep tree small and avoids the criterion-vs-real-world reporting
//! mismatch (criterion's "best of N" hides tail latency, but tail
//! latency is exactly what the p95 gate is about).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koda_sandbox::policy::SandboxPolicy;
use koda_sandbox::pool::SandboxPool;
use koda_sandbox::workspace::CwdProvider;

// ── Bench knobs ──────────────────────────────────────────────────────────────

/// Iterations for the warm bench. Big enough that p95/p99 are
/// statistically meaningful but small enough that the bench finishes
/// in a few seconds.
const WARM_ITERATIONS: usize = 1000;

/// Iterations for the cold bench. Each iteration spawns a fresh
/// worker process (fork+exec+handshake), so we can't afford 1000
/// without the bench taking minutes.
const WARM_BUCKET_SIZE: usize = 64;
const COLD_ITERATIONS: usize = 100;

/// The acceptance gate from #934 §12. If `acquire_slot_warm` exceeds
/// this at p95, the bench exits 1 — which is how this becomes a real
/// CI gate later without needing extra plumbing.
const WARM_P95_GATE: Duration = Duration::from_millis(15);

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    ensure_worker_bin();

    // Build a multi-thread runtime explicitly so we can drive the
    // pool without a #[tokio::main] attribute (benches don't support
    // attribute macros cleanly).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let warm = runtime.block_on(bench_warm());
    let cold = runtime.block_on(bench_cold());

    println!();
    println!("=== Verdict ===");
    let warm_pass = warm.p95 <= WARM_P95_GATE;
    if warm_pass {
        println!(
            "✅ acquire_slot_warm p95 = {:?} (gate: {:?})",
            warm.p95, WARM_P95_GATE
        );
    } else {
        println!(
            "❌ acquire_slot_warm p95 = {:?} EXCEEDS gate {:?}",
            warm.p95, WARM_P95_GATE
        );
    }
    println!(
        "   acquire_slot_cold p95 = {:?} (no gate; informational)",
        cold.p95
    );

    // Emit the JSON summary lines AFTER the verdict so a tail-reading
    // archival script can grab them in one pass.
    println!();
    println!(
        "BENCH_JSON: {}",
        warm.to_json("acquire_slot_warm", Some(WARM_P95_GATE))
    );
    println!("BENCH_JSON: {}", cold.to_json("acquire_slot_cold", None));

    if !warm_pass {
        std::process::exit(1);
    }
}

// ── Bench 1: warm acquire ────────────────────────────────────────────────────

async fn bench_warm() -> Stats {
    println!("=== Bench: acquire_slot_warm ===");
    println!(
        "Pre-warming pool with {WARM_BUCKET_SIZE} workers, then \
         timing {WARM_ITERATIONS} acquires..."
    );

    let policy = SandboxPolicy::default();
    let writable_root = std::env::temp_dir();
    let provider = Arc::new(CwdProvider::new(&writable_root));

    let pool = SandboxPool::new(WARM_BUCKET_SIZE);
    pool.warm_bucket(writable_root.clone(), &policy, None, WARM_BUCKET_SIZE)
        .await
        .expect("warm_bucket");

    // Quick sanity — if warming failed silently we'd be benching the
    // cold path and wondering why the gate fails.
    assert!(
        pool.idle_count() >= WARM_BUCKET_SIZE,
        "warm_bucket left only {} idle workers (wanted {WARM_BUCKET_SIZE})",
        pool.idle_count()
    );

    let mut samples = Vec::with_capacity(WARM_ITERATIONS);
    for i in 0..WARM_ITERATIONS {
        let slot_id = format!("warm-{i}");
        let t0 = Instant::now();
        let slot = pool
            .acquire(
                provider.clone(),
                writable_root.clone(),
                &policy,
                None,
                slot_id,
            )
            .await
            .expect("acquire warm");
        samples.push(t0.elapsed());

        // Drop returns the worker to the pool so the NEXT iteration
        // hits the warm path. This is the steady-state we care about.
        drop(slot);
    }

    let stats = Stats::from_samples(samples);
    stats.print(WARM_ITERATIONS);
    stats
}

// ── Bench 2: cold acquire ────────────────────────────────────────────────────

async fn bench_cold() -> Stats {
    println!();
    println!("=== Bench: acquire_slot_cold ===");
    println!("Empty pool ({COLD_ITERATIONS} iterations, each spawns a fresh worker)...");

    let policy = SandboxPolicy::default();
    let writable_root = std::env::temp_dir();
    let provider = Arc::new(CwdProvider::new(&writable_root));

    let mut samples = Vec::with_capacity(COLD_ITERATIONS);
    for i in 0..COLD_ITERATIONS {
        // Fresh pool each iteration so the worker is guaranteed cold.
        // Cheaper than `target_per_bucket=0` (which would still hit
        // the bucket-lookup path) and unambiguous about what we're
        // measuring.
        let pool = SandboxPool::new(1);
        let slot_id = format!("cold-{i}");
        let t0 = Instant::now();
        let slot = pool
            .acquire(
                provider.clone(),
                writable_root.clone(),
                &policy,
                None,
                slot_id,
            )
            .await
            .expect("acquire cold");
        samples.push(t0.elapsed());
        drop(slot);
    }

    let stats = Stats::from_samples(samples);
    stats.print(COLD_ITERATIONS);
    stats
}

// ── Stats ────────────────────────────────────────────────────────────────────

struct Stats {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
    mean: Duration,
}

impl Stats {
    /// Sort + percentile in the obvious way. We don't bother with
    /// fancy quantile estimation: 100-1000 samples is small enough
    /// that nearest-rank percentile is fine and reproducible.
    fn from_samples(mut samples: Vec<Duration>) -> Self {
        assert!(!samples.is_empty(), "empty sample set");
        samples.sort_unstable();
        let n = samples.len();
        Self {
            p50: samples[n * 50 / 100],
            p95: samples[n * 95 / 100],
            p99: samples[n.saturating_sub(1).min(n * 99 / 100)],
            max: *samples.last().unwrap(),
            mean: samples.iter().sum::<Duration>() / n as u32,
        }
    }

    fn print(&self, iterations: usize) {
        println!("  iterations: {iterations}");
        println!("  p50:  {:?}", self.p50);
        println!("  p95:  {:?}", self.p95);
        println!("  p99:  {:?}", self.p99);
        println!("  max:  {:?}", self.max);
        println!("  mean: {:?}", self.mean);
    }

    /// One-line JSON for archival. Hand-rolled because dragging in
    /// serde_json to a bench feels heavy and the schema is trivial.
    /// Times are nanos, not micros, so the warm bench (sub-µs p50)
    /// doesn't truncate to zero in the archived data.
    fn to_json(&self, name: &str, gate: Option<Duration>) -> String {
        let gate_ns = gate
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_else(|| "null".into());
        format!(
            r#"{{"name":"{name}","p50_ns":{},"p95_ns":{},"p99_ns":{},"max_ns":{},"mean_ns":{},"gate_ns":{}}}"#,
            self.p50.as_nanos(),
            self.p95.as_nanos(),
            self.p99.as_nanos(),
            self.max.as_nanos(),
            self.mean.as_nanos(),
            gate_ns,
        )
    }
}

// ── Worker binary discovery ──────────────────────────────────────────────────
//
// Same logic the test suite uses (see `koda-sandbox/src/pool/tests.rs`).
// Cargo doesn't set `CARGO_BIN_EXE_*` for benches, so we walk up from
// the bench binary's path looking for `target/{debug,release}/koda-fs-worker`.
// Without this, the pool's first `WorkerClient::spawn_*` panics with
// "koda-fs-worker not found" and the bench fails opaquely.

fn ensure_worker_bin() {
    if std::env::var("KODA_FS_WORKER_BIN").is_ok() {
        return;
    }
    let mut p: PathBuf = std::env::current_exe().expect("current_exe");
    while p.pop() {
        if p.ends_with("debug") || p.ends_with("release") {
            let bin = p.join("koda-fs-worker");
            if bin.exists() {
                // SAFETY: bench process is single-threaded at this
                // point (we haven't built the tokio runtime yet) and
                // we set this exactly once.
                unsafe {
                    std::env::set_var("KODA_FS_WORKER_BIN", &bin);
                }
                return;
            }
        }
    }
    panic!(
        "koda-fs-worker binary not found. Run `cargo build -p koda-sandbox \
         --bin koda-fs-worker` (or just `cargo bench` which does it for you) first."
    );
}
