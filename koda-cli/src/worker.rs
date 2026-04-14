//! `koda __worker` — sandboxed headless worker subprocess.
//!
//! This subcommand is **not user-facing**; it is spawned exclusively by the
//! supervisor ([`crate::ipc_supervisor::IpcSupervisor::spawn_worker`]) when
//! the user runs `koda -p "..."` (headless mode).
//!
//! ## Environment
//!
//! The supervisor passes configuration via env vars rather than CLI args so
//! the args remain clean for logging/monitoring:
//!
//! | Var | Purpose |
//! |-----|---------|
//! | `KODA_SUPERVISOR_SOCKET` | Unix socket path — detected by `create_provider` |
//! | `KODA_WORKER_PROMPT` | The user's prompt |
//! | `KODA_WORKER_SESSION_ID` | Pre-created session ID |
//! | `KODA_WORKER_PROJECT_ROOT` | Canonical project root |
//! | `KODA_WORKER_DB_PATH` | Full path to koda.db |
//! | `KODA_WORKER_OUTPUT_FORMAT` | `text` or `json` |
//! | `KODA_WORKER_AGENT` | Agent name (default: `"default"`) |
//! | `KODA_WORKER_MODE` | Trust mode: `safe` or `auto` |
//!
//! ## Network isolation
//!
//! The supervisor wraps the worker invocation in a network sandbox:
//! - **macOS**: `sandbox-exec` profile denying all network except unix sockets
//! - **Linux**: `unshare --net` (new network namespace, no interfaces)
//!
//! Unix sockets are filesystem-based and survive both sandboxes, so IPC back
//! to the supervisor works normally.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Run the headless worker.  Called when `koda __worker` is matched.
///
/// Reads all configuration from environment variables (set by the supervisor),
/// opens the database, and delegates to [`crate::headless::run_headless`].
pub async fn run_worker() -> Result<i32> {
    let prompt = required_env("KODA_WORKER_PROMPT")?;
    let session_id = required_env("KODA_WORKER_SESSION_ID")?;
    let project_root = PathBuf::from(required_env("KODA_WORKER_PROJECT_ROOT")?);
    let db_path = PathBuf::from(required_env("KODA_WORKER_DB_PATH")?);
    let output_format =
        std::env::var("KODA_WORKER_OUTPUT_FORMAT").unwrap_or_else(|_| "text".into());
    let agent = std::env::var("KODA_WORKER_AGENT").unwrap_or_else(|_| "default".into());
    let mode_str = std::env::var("KODA_WORKER_MODE").unwrap_or_else(|_| "safe".into());

    // Open the database passed from the supervisor (same file, separate pool).
    let db = koda_core::db::Database::open(&db_path)
        .await
        .with_context(|| format!("worker: open db {}", db_path.display()))?;

    // Load config from disk.  API keys were injected into the environment by
    // the supervisor; `query_and_apply_capabilities` is skipped here since the
    // worker has no outbound network (and model capabilities were already
    // queried by the supervisor before spawning).
    let mut config = koda_core::config::KodaConfig::load(&project_root, &agent)
        .with_context(|| "worker: load KodaConfig")?;
    let trust = koda_core::trust::TrustMode::parse(&mode_str).unwrap_or_default();
    config = config.with_trust(trust);

    // `create_provider` sees KODA_SUPERVISOR_SOCKET and returns IpcLlmProvider.
    // All LLM calls tunnel through the supervisor — no direct TCP needed.
    let exit_code =
        crate::headless::run_headless(project_root, config, db, session_id, prompt, &output_format)
            .await?;

    Ok(exit_code)
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("worker: required env var {key} is not set"))
}
