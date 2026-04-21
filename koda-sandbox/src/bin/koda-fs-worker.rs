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
//! ## Policy injection (Phase 2f)
//!
//! When spawned with `--root <path>` the worker enforces write-policy
//! for every `Write`/`Edit` request. The full [`SandboxPolicy`] is
//! passed via the `KODA_FS_WORKER_POLICY` environment variable as a
//! JSON string. Both are set by
//! [`koda_sandbox::worker_client::WorkerClient::spawn_with_policy`].
//!
//! If `--root` is absent the worker runs with no enforcement (legacy
//! behavior, used by `WorkerClient::spawn()` and stdio tests).
//!
//! ## Accepted arguments
//!
//! ```text
//! koda-fs-worker                                # stdin/stdout, no policy
//! koda-fs-worker --socket <sock>                # Unix socket, no policy
//! koda-fs-worker --socket <sock> --root <root>  # Unix socket + policy
//! ```
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

use anyhow::{Result, bail};
use koda_sandbox::policy::SandboxPolicy;
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("KODA_FS_WORKER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let Args {
        socket_path,
        writable_root,
    } = parse_args()?;

    // Read optional policy from env — absent means use permissive default.
    let policy = read_policy_env()?;

    match (socket_path, writable_root) {
        (Some(sock), Some(root)) => {
            // Production: Unix socket + write-policy enforcement.
            #[cfg(unix)]
            {
                koda_sandbox::worker::run_unix_socket_with_policy(&sock, root, policy).await
            }
            #[cfg(not(unix))]
            {
                bail!("--socket is only supported on Unix")
            }
        }
        (Some(sock), None) => {
            // Unix socket, no enforcement (legacy / test path).
            #[cfg(unix)]
            {
                koda_sandbox::worker::run_unix_socket(&sock).await
            }
            #[cfg(not(unix))]
            {
                bail!("--socket is only supported on Unix")
            }
        }
        (None, _) => {
            // stdin/stdout (legacy integration tests and `run_stdio`).
            koda_sandbox::worker::run_stdio().await
        }
    }
}

// ── Argument parsing ──────────────────────────────────────────────────────

struct Args {
    socket_path: Option<PathBuf>,
    writable_root: Option<PathBuf>,
}

/// Minimal flag parser for the two supported flags.
///
/// We intentionally avoid adding a CLI-parsing dep (clap et al.) to the
/// worker binary — the arg surface is tiny and the dep would meaningfully
/// bloat the binary's startup time for every slot spawn.
fn parse_args() -> Result<Args> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut socket_path: Option<PathBuf> = None;
    let mut writable_root: Option<PathBuf> = None;
    let mut iter = raw.iter();

    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--socket" => {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--socket requires a path argument"))?;
                socket_path = Some(PathBuf::from(val));
            }
            "--root" => {
                let val = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--root requires a path argument"))?;
                writable_root = Some(PathBuf::from(val));
            }
            other => bail!(
                "unknown argument: {other}\nusage: koda-fs-worker [--socket <path>] [--root <path>]"
            ),
        }
    }

    Ok(Args {
        socket_path,
        writable_root,
    })
}

// ── Policy env var ────────────────────────────────────────────────────────

/// Read `KODA_FS_WORKER_POLICY` and deserialize it as a [`SandboxPolicy`].
///
/// Returns [`SandboxPolicy::default()`] when the variable is unset \u2014 this
/// produces a permissive policy that only enforces the absolute deny list
/// (koda credential DB and dangerous system paths).
///
/// Returns an error when the variable is set but cannot be parsed \u2014 a
/// malformed policy env var is a programming error in the host, not a
/// user error, so we fail hard rather than silently falling back.
fn read_policy_env() -> Result<SandboxPolicy> {
    match std::env::var("KODA_FS_WORKER_POLICY") {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("KODA_FS_WORKER_POLICY is not valid JSON: {e}")),
        Err(std::env::VarError::NotPresent) => Ok(SandboxPolicy::default()),
        Err(e) => Err(anyhow::anyhow!("KODA_FS_WORKER_POLICY env var error: {e}")),
    }
}
