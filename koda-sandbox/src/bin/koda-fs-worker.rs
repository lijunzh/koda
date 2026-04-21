//! `koda-fs-worker` — long-lived FS worker process spawned by the
//! sandbox shim (Phase 2c of #934).
//!
//! ## Transport selection
//!
//! - **`--socket <path>`** (production): bind a Unix domain socket at
//!   `<path>`, write "ready\n" to stdout, accept one connection,
//!   serve it. Used by [`koda_sandbox::worker_client::WorkerClient`].
//! - **No arguments** (legacy / tests): run against stdin/stdout.
//!   The `worker_binary` integration tests still use this path.
//!
//! ## Lifecycle
//!
//! Spawned by `WorkerClient::spawn()` for each sandbox slot. Exits
//! when the host closes the connection (clean EOF) or sends
//! [`koda_sandbox::ipc::Request::Shutdown`].
//!
//! ## Why a binary not a library function?
//!
//! Crash isolation. A panicking FS handler kills the worker, not the
//! host process driving the LLM session.

use anyhow::Result;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KODA_FS_WORKER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // Parse --socket <path> from argv. We roll our own to avoid adding
    // a CLI-parsing dep to this binary — two-argument argv parsing is
    // not worth the dep weight.
    let args: Vec<String> = std::env::args().collect();
    let socket_path = match args.as_slice() {
        [_, flag, path] if flag == "--socket" => Some(std::path::PathBuf::from(path)),
        [_] => None,
        _ => anyhow::bail!("usage: koda-fs-worker [--socket <path>]"),
    };

    match socket_path {
        Some(path) => {
            #[cfg(unix)]
            {
                koda_sandbox::worker::run_unix_socket(&path).await
            }
            #[cfg(not(unix))]
            {
                anyhow::bail!("--socket is only supported on Unix")
            }
        }
        None => koda_sandbox::worker::run_stdio().await,
    }
}
