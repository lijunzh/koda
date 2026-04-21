//! `koda-sandbox` — capability-aware sandbox layer for Koda.
//!
//! Per the design in <https://github.com/lijunzh/koda/issues/934>: this
//! crate owns *kernel-level* sandbox enforcement (Seatbelt on macOS, bwrap
//! on Linux), workspace provisioning, and the egress proxy (Phase 3+).
//!
//! ## Phase 0 — what's here
//!
//! - [`SandboxPolicy`] data types (issue §4.2 schema).
//! - [`SandboxRuntime`] trait + per-platform impls.
//! - [`SandboxRuntime::transform`] entry point: pure `(cmd, policy) → SandboxExecRequest`.
//! - [`WorkspaceProvider`] trait + [`CwdProvider`] impl.
//!
//! Phase 0 is a *refactor* — the runtime ignores the policy fields and
//! reproduces pre-#934 behavior byte-for-byte. Subsequent phases consume
//! the policy fields incrementally.
//!
//! ## Dependency direction
//!
//! ```text
//! koda-cli → koda-core → koda-sandbox
//!                          │
//!                          └─ no upward dependency
//! ```
//!
//! `koda-sandbox` knows nothing about `Persistence`, `Provider`,
//! `ToolRegistry`. Pure infrastructure — testable with
//! `assert_eq!(transform(cmd, policy), expected)`.

#![deny(missing_docs)]

#[cfg(target_os = "linux")]
pub mod bwrap;
pub mod defaults;
pub mod ipc;
pub mod monitor;
pub mod policy;
pub mod policy_check;
#[cfg(target_os = "macos")]
pub mod seatbelt;
pub mod violations;
pub mod worker;
pub mod workspace;

pub use policy::{
    DomainPattern, FsPolicy, MitmConfig, NetPolicy, PathPattern, ResourceLimits, SandboxPolicy,
    TrustPreference,
};
pub use policy_check::is_fully_denied;
pub use violations::{
    DEFAULT_RING_CAPACITY, SandboxViolationStore, Violation, ViolationKind, global_store,
    render_block,
};
pub use workspace::{CwdProvider, WorkspaceProvider};

use anyhow::Result;
use std::path::Path;
use tokio::process::Command;

// ── Public API: SandboxRuntime trait ─────────────────────────────────────────

/// Inputs to [`SandboxRuntime::transform`].
///
/// Borrowed-only so callers can build a request on the stack without
/// allocating. The runtime returns a fully-spawnable [`SandboxExecRequest`]
/// that owns its data.
#[derive(Debug)]
pub struct SandboxTransformRequest<'a> {
    /// The command to run, as `sh -c "{command}"`. Quoting is the
    /// caller's responsibility.
    pub command: &'a str,

    /// Working directory + writable-root anchor. Most policy paths are
    /// resolved relative to this.
    pub project_root: &'a Path,

    /// Capability policy. Phase 0 ignores all fields; later phases
    /// progressively enforce them.
    pub policy: &'a SandboxPolicy,
}

/// Output of [`SandboxRuntime::transform`].
///
/// Wraps a `tokio::process::Command` so callers can attach stdio, piping,
/// and cancellation tokens before spawning.
#[derive(Debug)]
pub struct SandboxExecRequest {
    /// Spawnable command — `sandbox-exec ...` on macOS, `bwrap ...` on
    /// Linux, plain `sh -c` on platforms with no backend.
    pub command: Command,
}

/// Health/dependency check report. Returned by
/// [`SandboxRuntime::check_dependencies`] so callers (e.g. `/sandbox status`
/// CLI command) can surface actionable diagnostics.
#[derive(Debug, Clone)]
pub struct DependencyReport {
    /// Backend identifier — `"seatbelt"`, `"bwrap"`, or `"none"`.
    pub backend: &'static str,
    /// Whether the backend is installed and functional.
    pub available: bool,
    /// Human-readable reason when `available` is `false`.
    pub reason: Option<String>,
}

/// Backend-agnostic sandbox runtime.
///
/// Phase 0 has [`Self::transform`] + [`Self::check_dependencies`]; Phase 2
/// will add `acquire(&self, policy) -> SandboxSlot` for the long-lived
/// per-agent slot model.
///
/// Violation tracking lives outside the trait: see [`violations::global_store`]
/// for the process-wide ring buffer and [`monitor::parse_stderr`] for
/// the heuristic stderr parser used by the bash tool.
pub trait SandboxRuntime: Send + Sync {
    /// Compile a high-level command + policy into a concrete spawnable
    /// `Command`. Pure-ish (consults `$HOME`, canonicalizes paths) but
    /// performs no I/O on the supplied command.
    ///
    /// # Errors
    ///
    /// Returns an error if the policy or paths violate runtime invariants
    /// (e.g. unsafe characters in a path that would break the seatbelt
    /// profile syntax).
    fn transform(&self, req: SandboxTransformRequest<'_>) -> Result<SandboxExecRequest>;

    /// Probe whether the backend is functional. Cached after first call.
    fn check_dependencies(&self) -> DependencyReport;
}

// ── Per-platform runtime impls ───────────────────────────────────────────────

/// macOS Seatbelt runtime — uses `sandbox-exec` with an inline profile.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub struct SeatbeltRuntime;

#[cfg(target_os = "macos")]
impl SandboxRuntime for SeatbeltRuntime {
    fn transform(&self, req: SandboxTransformRequest<'_>) -> Result<SandboxExecRequest> {
        let command = seatbelt::build_command(req.command, req.project_root, req.policy)?;
        Ok(SandboxExecRequest { command })
    }

    fn check_dependencies(&self) -> DependencyReport {
        if seatbelt::is_available() {
            DependencyReport {
                backend: "seatbelt",
                available: true,
                reason: None,
            }
        } else {
            DependencyReport {
                backend: "seatbelt",
                available: false,
                reason: Some(
                    "sandbox-exec failed to run a probe command. Check macOS \
                     SIP / TCC restrictions."
                        .into(),
                ),
            }
        }
    }
}

/// Linux bwrap runtime — uses `bwrap` (bubblewrap) with mount namespace
/// isolation.
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct BwrapRuntime;

#[cfg(target_os = "linux")]
impl SandboxRuntime for BwrapRuntime {
    fn transform(&self, req: SandboxTransformRequest<'_>) -> Result<SandboxExecRequest> {
        let command = bwrap::build_command(req.command, req.project_root, req.policy)?;
        Ok(SandboxExecRequest { command })
    }

    fn check_dependencies(&self) -> DependencyReport {
        if bwrap::is_available() {
            DependencyReport {
                backend: "bwrap",
                available: true,
                reason: None,
            }
        } else {
            DependencyReport {
                backend: "bwrap",
                available: false,
                reason: Some(
                    "bwrap not installed or unable to create user namespaces. \
                     Install: apt install bubblewrap | dnf install bubblewrap"
                        .into(),
                ),
            }
        }
    }
}

/// Fallback runtime for platforms without a kernel sandbox backend.
///
/// `transform()` returns a plain `sh -c` Command. Trips `available: false`
/// in [`SandboxRuntime::check_dependencies`] so callers can surface a
/// "running unsandboxed" warning to the user.
#[derive(Debug, Default)]
pub struct UnsandboxedRuntime;

impl SandboxRuntime for UnsandboxedRuntime {
    fn transform(&self, req: SandboxTransformRequest<'_>) -> Result<SandboxExecRequest> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(req.command)
            .current_dir(req.project_root);
        Ok(SandboxExecRequest { command })
    }

    fn check_dependencies(&self) -> DependencyReport {
        DependencyReport {
            backend: "none",
            available: false,
            reason: Some(
                "No kernel sandbox backend available on this platform. \
                 Commands run unsandboxed."
                    .into(),
            ),
        }
    }
}

// ── Convenience: pick the right runtime for the host ────────────────────────

/// Returns the platform-appropriate runtime.
///
/// On macOS: always returns `SeatbeltRuntime` (sandbox-exec ships with
/// the OS).
///
/// On Linux: returns `BwrapRuntime` when `bwrap` is functional, otherwise
/// [`UnsandboxedRuntime`] with a tracing warning. The sandbox is best-effort
/// — we never block the user just because the kernel enforcement layer is
/// missing.
///
/// On other platforms: [`UnsandboxedRuntime`].
#[must_use]
pub fn current_runtime() -> Box<dyn SandboxRuntime> {
    #[cfg(target_os = "macos")]
    {
        Box::new(SeatbeltRuntime)
    }
    #[cfg(target_os = "linux")]
    {
        if bwrap::is_available() {
            Box::new(BwrapRuntime)
        } else {
            Box::new(UnsandboxedRuntime)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(UnsandboxedRuntime)
    }
}

/// Returns `true` if the platform sandbox backend is available.
///
/// Convenience wrapper around `current_runtime().check_dependencies()`
/// for callers (like the trust layer) that only need a yes/no.
#[must_use]
pub fn is_available() -> bool {
    current_runtime().check_dependencies().available
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsandboxed_runtime_produces_sh_command() {
        let policy = SandboxPolicy::default();
        let req = SandboxTransformRequest {
            command: "echo hi",
            project_root: Path::new("/tmp"),
            policy: &policy,
        };
        let runtime = UnsandboxedRuntime;
        let result = runtime.transform(req).unwrap();
        // sh -c "echo hi" — verify by inspecting the program.
        let program = result.command.as_std().get_program();
        assert_eq!(program, "sh");
    }

    #[test]
    fn unsandboxed_runtime_reports_unavailable() {
        let report = UnsandboxedRuntime.check_dependencies();
        assert_eq!(report.backend, "none");
        assert!(!report.available);
        assert!(report.reason.is_some());
    }

    #[test]
    fn current_runtime_is_constructible() {
        // Smoke test — we just want to confirm there's *some* runtime
        // returned, not panic on this platform. The actual backend depends
        // on the test host.
        let runtime = current_runtime();
        let _report = runtime.check_dependencies(); // doesn't panic
    }
}
