//! `koda-fs-worker` — long-lived FS worker process spawned by the
//! sandbox shim (Phase 2a of #934).
//!
//! Runs the [`koda_sandbox::worker::run_stdio`] dispatch loop against
//! process stdin/stdout. Phase 2c will add a Unix-socket variant for
//! the production per-slot wiring; stdio is what the integration tests
//! use today.
//!
//! ## Lifecycle
//!
//! Spawned by `koda_sandbox::worker_client` (Phase 2c) for each
//! sandbox slot. Exits when:
//! - The host closes stdin (EOF) — normal "slot retired" path.
//! - The host sends [`koda_sandbox::ipc::Request::Shutdown`] — graceful
//!   shutdown with ack.
//! - A transport-level IO error occurs — exit 1, host respawns.
//!
//! ## Why a binary not a library function?
//!
//! Crash isolation. A panicking FS handler kills the worker, not the
//! host process driving the LLM session. The host wraps respawn logic
//! in Phase 2c.

use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Single-threaded runtime: this binary is one process per sandbox
    // slot serving one host. No need for the multi-threaded scheduler's
    // overhead — every request is handled sequentially anyway (the
    // host serializes them by waiting for each response).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KODA_FS_WORKER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    koda_sandbox::worker::run_stdio().await
}
