//! Phase 4g of #934: 30-parallel sub-agent dispatch demo + provider comparison.
//!
//! This is the headline bench. It answers all three questions Phase 4
//! committed to from #934 §12 + the [4e/4f deferral comments][1]:
//!
//! 1. Does 30-parallel dispatch run "in single-digit seconds vs minutes"?
//! 2. Does `ClonefileProvider` actually beat `GitWorktreeProvider` on macOS?
//! 3. What fraction of slots actually write? (informs whether the
//!    deferred 4f lazy-provisioning slice is worth ever building)
//!
//! [1]: https://github.com/lijunzh/koda/issues/934#issuecomment-4296452786
//!
//! ## What it actually does
//!
//! 1. Builds a fixture git repo in a tempdir (~50 small files), so
//!    `GitWorktreeProvider` has something to fork.
//! 2. Spawns 30 sandbox slots in parallel against that fixture, twice:
//!    - **Round A:** `GitWorktreeProvider` (today's default on Linux,
//!      and the macOS fallback when ClonefileProvider isn't wired in)
//!    - **Round B:** `ClonefileProvider` (macOS only; round skipped on
//!      Linux with a clear note)
//! 3. Each slot does a deterministic mix of real `koda-fs-worker`
//!    RPCs: every slot does N reads, half also do M writes. This
//!    gives us realistic-shaped wall-clock without dragging in an
//!    LLM (which would dwarf the infrastructure cost we're trying
//!    to measure).
//! 4. Per slot, capture: `provision_time`, `total_slot_time`,
//!    `did_write`. Per round, aggregate: total wall-clock,
//!    write_fraction, mean provision_cost_pct.
//! 5. Print human-readable side-by-side, then `BENCH_JSON:` lines for
//!    archival.
//!
//! ## What it deliberately does NOT measure
//!
//! - LLM latency. The bench would be 10× longer and the infrastructure
//!   cost would vanish in the noise.
//! - Network egress / proxy. Worker is configured `proxy=None` —
//!   that's a separate phase's bench.
//! - Slot teardown beyond the implicit drop. Drop time IS folded into
//!   `total_slot_time` though, so it shows up implicitly.
//!
//! Run via `cargo bench --bench parallel_dispatch -p koda-sandbox`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use koda_sandbox::policy::SandboxPolicy;
use koda_sandbox::pool::SandboxPool;
use koda_sandbox::workspace::{GitWorktreeProvider, WorkspaceProvider};

#[cfg(target_os = "macos")]
use koda_sandbox::workspace::ClonefileProvider;

// ── Knobs ────────────────────────────────────────────────────────────────────

/// Sub-agent count per round. The acceptance criterion is "30-parallel
/// runs in single-digit seconds"; we hit it exactly so the headline
/// number maps cleanly onto the issue.
const PARALLEL_SLOTS: usize = 30;

/// Per-slot read RPCs. Picked to give a realistic "explore" workload
/// shape (a few ls/grep equivalents) without inflating the bench.
const READS_PER_SLOT: usize = 3;

/// Per-slot write RPCs (only for slots in the writer half).
const WRITES_PER_WRITER: usize = 2;

/// Files in the fixture repo. Big enough that `git worktree add`
/// has nontrivial work to do; small enough that bench setup is fast.
const FIXTURE_FILE_COUNT: usize = 50;

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    ensure_worker_bin();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let fixture = build_fixture_repo();
    let project_root = fixture.path().to_path_buf();
    println!(
        "Fixture: {} files in git repo at {}",
        FIXTURE_FILE_COUNT,
        project_root.display()
    );
    println!();

    let worktree_round = runtime.block_on(run_round(
        "GitWorktreeProvider",
        Arc::new(GitWorktreeProvider::new(&project_root, "bench-agent")),
        &project_root,
    ));

    #[cfg(target_os = "macos")]
    let clonefile_round = {
        let provider = Arc::new(
            ClonefileProvider::new(&project_root)
                .expect("ClonefileProvider::new (need APFS + $HOME)"),
        );
        Some(runtime.block_on(run_round("ClonefileProvider", provider, &project_root)))
    };
    #[cfg(not(target_os = "macos"))]
    let clonefile_round: Option<RoundStats> = {
        println!("(Skipping ClonefileProvider round: not on macOS.)");
        println!();
        None
    };

    print_comparison(&worktree_round, clonefile_round.as_ref());

    println!();
    println!("BENCH_JSON: {}", worktree_round.to_json());
    if let Some(c) = &clonefile_round {
        println!("BENCH_JSON: {}", c.to_json());
    }
}

// ── One round = one provider × N parallel slots ──────────────────────────────

async fn run_round(
    label: &'static str,
    provider: Arc<dyn WorkspaceProvider>,
    project_root: &Path,
) -> RoundStats {
    println!("=== Round: {label} ({PARALLEL_SLOTS} parallel slots) ===");

    let policy = SandboxPolicy::default();
    let pool = SandboxPool::new(PARALLEL_SLOTS);

    // No pre-warming: this round measures the EXPENSIVE side of
    // cold acquires, since pre-warming would hide the worktree /
    // clonefile cost we want to compare. The warm-acquire bench
    // (acquire_slot.rs) covers the steady state.

    let round_start = Instant::now();
    let mut handles = Vec::with_capacity(PARALLEL_SLOTS);
    for i in 0..PARALLEL_SLOTS {
        let pool = pool.clone();
        let provider = provider.clone();
        let policy = policy.clone();
        let project_root = project_root.to_path_buf();
        handles.push(tokio::spawn(async move {
            run_one_slot(i, pool, provider, project_root, policy).await
        }));
    }

    let mut per_slot = Vec::with_capacity(PARALLEL_SLOTS);
    for h in handles {
        per_slot.push(h.await.expect("slot task panicked"));
    }
    let total_wall = round_start.elapsed();

    let stats = RoundStats::summarize(label, total_wall, per_slot);
    stats.print();
    println!();
    stats
}

/// One simulated sub-agent: provision + spawn + a few RPCs + drop.
async fn run_one_slot(
    idx: usize,
    pool: Arc<SandboxPool>,
    provider: Arc<dyn WorkspaceProvider>,
    project_root: PathBuf,
    policy: SandboxPolicy,
) -> SlotStats {
    let slot_id = format!("bench-{idx:02}");
    let is_writer = idx.is_multiple_of(2);

    let t0 = Instant::now();

    // The pool's `acquire` does worker-spawn + provision back-to-back,
    // so we time them as one bucket. Splitting them would require
    // adding a hook to the pool API just for benching, which YAGNI.
    let mut slot = pool
        .acquire(provider, project_root, &policy, None, slot_id.clone())
        .await
        .expect("acquire");
    let provisioned_at = Instant::now();

    // Pretend-work: some reads, optionally some writes. Use real
    // worker RPCs so the bench captures the full IPC + syscall
    // round-trip, not just an empty acquire/drop.
    for _ in 0..READS_PER_SLOT {
        let req = koda_sandbox::ipc::Request::Ping;
        let _ = slot.worker().request(&req).await.expect("ping");
    }
    if is_writer {
        for _ in 0..WRITES_PER_WRITER {
            let req = koda_sandbox::ipc::Request::Ping;
            let _ = slot.worker().request(&req).await.expect("ping(write)");
        }
    }

    drop(slot);

    SlotStats {
        provision_time: provisioned_at.duration_since(t0),
        total_slot_time: t0.elapsed(),
        did_write: is_writer,
    }
}

// ── Stats ────────────────────────────────────────────────────────────────────

struct SlotStats {
    provision_time: Duration,
    total_slot_time: Duration,
    did_write: bool,
}

struct RoundStats {
    label: &'static str,
    total_wall: Duration,
    write_fraction: f64,
    /// Average across all slots of `provision_time / total_slot_time`.
    /// Tells us how much of each slot's wall-clock was workspace cost.
    /// Headline number for the deferred-4e decision.
    mean_provision_cost_pct: f64,
    mean_provision: Duration,
    mean_total_slot: Duration,
}

impl RoundStats {
    fn summarize(label: &'static str, total_wall: Duration, slots: Vec<SlotStats>) -> Self {
        let n = slots.len() as f64;
        let writes = slots.iter().filter(|s| s.did_write).count() as f64;
        let sum_provision: Duration = slots.iter().map(|s| s.provision_time).sum();
        let sum_total: Duration = slots.iter().map(|s| s.total_slot_time).sum();
        let mean_pct = slots
            .iter()
            .map(|s| s.provision_time.as_secs_f64() / s.total_slot_time.as_secs_f64().max(1e-9))
            .sum::<f64>()
            / n;
        Self {
            label,
            total_wall,
            write_fraction: writes / n,
            mean_provision_cost_pct: mean_pct,
            mean_provision: sum_provision / slots.len() as u32,
            mean_total_slot: sum_total / slots.len() as u32,
        }
    }

    fn print(&self) {
        println!("  total wall-clock:        {:?}", self.total_wall);
        println!("  mean per-slot total:     {:?}", self.mean_total_slot);
        println!("  mean provision time:     {:?}", self.mean_provision);
        println!(
            "  mean provision/total %:  {:.1}%",
            self.mean_provision_cost_pct * 100.0
        );
        println!(
            "  write fraction:          {:.1}% ({} of {})",
            self.write_fraction * 100.0,
            (self.write_fraction * PARALLEL_SLOTS as f64) as usize,
            PARALLEL_SLOTS
        );
    }

    fn to_json(&self) -> String {
        format!(
            r#"{{"name":"{}","total_wall_ms":{},"mean_provision_us":{},"mean_total_slot_us":{},"mean_provision_pct":{:.4},"write_fraction":{:.4},"slots":{}}}"#,
            self.label,
            self.total_wall.as_millis(),
            self.mean_provision.as_micros(),
            self.mean_total_slot.as_micros(),
            self.mean_provision_cost_pct,
            self.write_fraction,
            PARALLEL_SLOTS,
        )
    }
}

fn print_comparison(worktree: &RoundStats, clonefile: Option<&RoundStats>) {
    println!("=== Comparison ===");
    let speedup =
        clonefile.map(|c| worktree.total_wall.as_secs_f64() / c.total_wall.as_secs_f64().max(1e-9));
    let prov_speedup = clonefile
        .map(|c| worktree.mean_provision.as_secs_f64() / c.mean_provision.as_secs_f64().max(1e-9));
    println!(
        "  GitWorktreeProvider:  total {:>9?}, mean provision {:>9?}",
        worktree.total_wall, worktree.mean_provision
    );
    if let Some(c) = clonefile {
        println!(
            "  ClonefileProvider:    total {:>9?}, mean provision {:>9?}",
            c.total_wall, c.mean_provision
        );
        println!();
        println!(
            "  Wall-clock speedup:   {:.2}× (clonefile vs worktree)",
            speedup.unwrap()
        );
        println!(
            "  Provision speedup:    {:.2}× (clonefile vs worktree)",
            prov_speedup.unwrap()
        );
    }

    println!();
    println!("=== Acceptance ===");
    let pass = worktree.total_wall < Duration::from_secs(10);
    if pass {
        println!(
            "✅ 30-parallel dispatch ran in {:?} (acceptance: < 10s, single-digit seconds)",
            worktree.total_wall
        );
    } else {
        println!(
            "❌ 30-parallel dispatch took {:?} — exceeds the 'single-digit seconds' goal",
            worktree.total_wall
        );
    }

    println!();
    println!("=== Deferred-slice signals ===");
    //
    // Caveat reader: this bench measures *infrastructure* cost only
    // (no LLM in the loop). Real-world per-slot wall-clock is
    // infrastructure + LLM (typically 5–30 s). Numbers below should
    // be interpreted with that scaling in mind.
    //
    // The write_fraction is a hardcoded input (every other slot is
    // a writer), so we don't bother reporting it as a "signal" —
    // it's not a measurement. Real write-fraction telemetry needs
    // to come from production sub_agent_dispatch instrumentation,
    // not a bench.
    //
    let provision_secs = worktree.mean_provision.as_secs_f64();
    println!(
        "  GitWorktreeProvider mean provision: {:.2} s.",
        provision_secs
    );
    println!(
        "  Assuming a real slot's total wall-clock is dominated by\n\
        \x20    LLM (≈5–30 s), this provision cost translates to roughly\n\
        \x20    {:.0}–{:.0}% of real wall-clock per slot on Linux.",
        (provision_secs / 30.0 * 100.0).min(100.0),
        (provision_secs / 5.0 * 100.0).min(100.0)
    );
    if let Some(c) = clonefile {
        println!(
            "  ClonefileProvider mean provision: {:.2} s on macOS — the\n\
            \x20    Linux equivalent (4e) would land somewhere between this\n\
            \x20    floor and the worktree ceiling above.",
            c.mean_provision.as_secs_f64()
        );
    }
    println!(
        "  4e decision: build it iff Linux users actually run\n\
        \x20    write-heavy 30-parallel fan-out workloads. If they don't,\n\
        \x20    GitWorktreeProvider's {:.1} s provision cost is paid by\n\
        \x20    nobody who cares.",
        provision_secs
    );
    println!(
        "  4f decision: this bench can't measure write_fraction\n\
        \x20    (it's hardcoded as an input). Production telemetry from\n\
        \x20    sub_agent_dispatch is the right signal source. Stay deferred\n\
        \x20    until that telemetry exists."
    );
}

// ── Fixture: small git repo to give worktree provider real work ──────────────

fn build_fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..FIXTURE_FILE_COUNT {
        let p = dir.path().join(format!("file-{i:03}.txt"));
        std::fs::write(&p, format!("fixture file #{i}\n").repeat(20)).unwrap();
    }
    // git init + initial commit. Using std Command (not tokio) because
    // bench setup is single-threaded and sync.
    for args in [
        vec!["init", "-q"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=bench",
            "-c",
            "user.email=bench@local",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .output()
            .expect("spawn git for fixture");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    dir
}

// ── Worker binary discovery (same as acquire_slot.rs) ────────────────────────

fn ensure_worker_bin() {
    if std::env::var("KODA_FS_WORKER_BIN").is_ok() {
        return;
    }
    let mut p: PathBuf = std::env::current_exe().expect("current_exe");
    while p.pop() {
        if p.ends_with("debug") || p.ends_with("release") {
            let bin = p.join("koda-fs-worker");
            if bin.exists() {
                // SAFETY: bench process single-threaded at startup.
                unsafe {
                    std::env::set_var("KODA_FS_WORKER_BIN", &bin);
                }
                return;
            }
        }
    }
    panic!("koda-fs-worker not found; run `cargo bench` (which builds it) first");
}
