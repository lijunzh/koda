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
#[cfg(target_os = "linux")]
pub mod bwrap_proxy;
pub mod defaults;
pub mod fs;
pub mod ipc;
pub mod monitor;
pub mod path_defense;
pub mod policy;
pub mod policy_check;
#[cfg(unix)]
pub mod pool;
pub mod proxy;
pub mod rlimits;
#[cfg(target_os = "macos")]
pub mod seatbelt;
#[cfg(target_os = "macos")]
pub mod seatbelt_cache;
#[cfg(target_os = "linux")]
pub mod stage2;
pub mod violations;
pub mod worker;
#[cfg(unix)]
pub mod worker_client;
pub mod workspace;

pub use path_defense::{
    find_symlink_in_path, is_dangerous_system_path, is_path_inside, paths_for_write_check,
    resolve_deepest_existing_ancestor,
};
pub use policy::{
    DomainPattern, FsPolicy, MitmConfig, NetPolicy, PathPattern, ResourceLimits, SandboxPolicy,
    TrustPreference,
};
pub use policy_check::is_fully_denied;
pub use proxy::{
    BuiltInProxy, BuiltInSocks5Proxy, DEFAULT_DEV_ALLOWLIST, DEFAULT_NO_PROXY, ExternalProxy,
    Filter, PROXY_PORT_ENV_KEY, ProxyHandle, Socks5Server, ca_bundle_for_policy, proxy_env_vars,
    socks5_env_vars,
};
pub use violations::{
    DEFAULT_RING_CAPACITY, SandboxViolationStore, Violation, ViolationKind, global_store,
    render_block,
};
pub use workspace::{CwdProvider, GitWorktreeProvider, WorkspaceProvider};

// macOS-only: APFS clonefile-backed workspace provider. Re-exported
// at the crate root so consumers (notably `koda-core::sub_agent_dispatch`)
// don't have to repeat the cfg gate at the import site.
#[cfg(target_os = "macos")]
pub use workspace::ClonefileProvider;

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

    /// Loopback port of the per-session HTTP CONNECT proxy spawned by
    /// [`crate::proxy::BuiltInProxy::spawn`].
    ///
    /// **Phase 3c semantics:** when `Some(port)`, the runtime is asked
    /// to **kernel-enforce** that the only TCP outbound permitted is to
    /// `127.0.0.1:port` (plus loopback inbound for debuggers / dev
    /// servers). Today the seatbelt backend honors this via
    /// `seatbelt::build_command_with_proxy` (macOS only); the bwrap
    /// backend logs a one-time warning and falls back to env-var-only
    /// enforcement (Phase 3c.1 will design Linux kernel-enforcement
    /// separately, likely via slirp4netns or rootlesskit).
    ///
    /// `None` preserves the pre-3c open-network baseline — used for
    /// callers that haven't migrated, and as the regression-guard
    /// fallback.
    pub proxy_port: Option<u16>,
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
        // Phase 3c: when a proxy port is supplied, switch to the
        // kernel-enforced proxied profile (`build_command_with_proxy`).
        // The seatbelt SBPL denies all TCP outbound except the loopback
        // proxy port, so even ill-behaved binaries that ignore
        // `HTTPS_PROXY` env vars cannot escape via direct TCP.
        //
        // `allow_local_binding` and `weaker_macos_isolation` come
        // straight from the policy. Localhost binds (e.g. dev servers
        // on 127.0.0.1:3000) are still allowed by
        // [`network_proxied_rules`] regardless; the bind flag only
        // gates wildcard `*:*`. The trustd flag (Phase 3e of #934) is
        // off by default and only set by callers that need Go-style
        // out-of-process TLS validation to work inside the sandbox.
        let command = match req.proxy_port {
            Some(port) => seatbelt::build_command_with_proxy(
                req.command,
                req.project_root,
                req.policy,
                port,
                req.policy.net.allow_local_binding,
                req.policy.net.weaker_macos_isolation,
            )?,
            None => seatbelt::build_command(req.command, req.project_root, req.policy)?,
        };
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
        // Phase 3c.1.d: try kernel-enforced egress via the stage 2
        // helper. Falls back to env-var-only if any prerequisite is
        // missing (no proxy, bridge spawn failed, stage 2 binary
        // unlocatable). The fallback path is the same enforcement
        // tier as Phase 3b — well-behaved clients (curl, gh, npm,
        // pip, cargo, go, node, python) honor HTTPS_PROXY and route
        // through the proxy; ill-behaved binaries can still reach
        // the network. Every fallback emits the once-per-process
        // warning so the gap stays visible.
        let command = match try_build_kernel_enforced(&req) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => {
                if req.proxy_port.is_some() {
                    warn_proxy_unenforced_on_linux_once();
                }
                bwrap::build_command(req.command, req.project_root, req.policy)?
            }
            Err(e) => {
                tracing::warn!(
                    "koda-sandbox: kernel-enforced egress setup failed ({e:#}); \
                     falling back to env-var enforcement."
                );
                warn_proxy_unenforced_on_linux_once();
                bwrap::build_command(req.command, req.project_root, req.policy)?
            }
        };
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

/// Try to build a kernel-enforced (Phase 3c.1) bwrap command.
///
/// Returns:
/// - `Ok(Some(cmd))` — prerequisites met, kernel-enforced path active.
/// - `Ok(None)` — prerequisites missing (no proxy port, or no UDS
///   bridge spawned). Caller falls back to env-var-only enforcement.
/// - `Err(_)` — prerequisites partially met but stage 2 binary
///   couldn't be located. Caller logs and falls back.
///
/// The UDS path is derived deterministically from the proxy port +
/// our own PID via [`crate::bwrap_proxy::proxy_uds_path`] (same
/// formula the host bridge uses). No IPC needed to discover it.
#[cfg(target_os = "linux")]
fn try_build_kernel_enforced(
    req: &SandboxTransformRequest<'_>,
) -> Result<Option<tokio::process::Command>> {
    use anyhow::Context as _;
    let Some(port) = req.proxy_port else {
        return Ok(None);
    };
    let uds = bwrap_proxy::proxy_uds_path(std::process::id(), port);
    if !uds.exists() {
        // Bridge didn't spawn this session (BuiltInProxy::spawn would
        // have warned). Fall back silently — no second warning.
        return Ok(None);
    }
    let stage2 = bwrap::stage2_binary().context("locate koda-sandbox-stage2")?;
    let cmd = bwrap::build_command_with_proxy(
        req.command,
        req.project_root,
        req.policy,
        port,
        &uds,
        &stage2,
    )?;
    Ok(Some(cmd))
}

/// One-time warning when koda-core asks the bwrap backend to enforce a
/// proxy port but bwrap can't kernel-enforce it for this session
/// (e.g. UDS bridge spawn failed). The env-var bouquet still routes
/// well-behaved clients through the proxy; only ill-behaved binaries
/// that ignore `HTTPS_PROXY` escape the filter.
///
/// Lives at module scope (not inside `BwrapRuntime`) because the
/// `OnceLock` must outlive any single runtime instance — each
/// `KodaSession` builds a fresh `BwrapRuntime`, but the gap is the
/// same on every invocation, so once-per-process is the right cadence.
#[cfg(target_os = "linux")]
fn warn_proxy_unenforced_on_linux_once() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "koda-sandbox: built-in proxy is active but the kernel-enforced \
             egress path didn't activate for this session (UDS bridge or \
             stage 2 helper unavailable). Well-behaved HTTP clients (curl, \
             gh, npm, pip, cargo, go, node, python) honor HTTPS_PROXY and \
             route through the filter; ill-behaved binaries can still reach \
             the network directly."
        );
    });
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
            proxy_port: None,
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

    /// Phase 3c: when `proxy_port = None`, the request runs through the
    /// unsandboxed runtime unchanged — same `sh` invocation, no env or
    /// arg surface added. Regression guard against the request gaining
    /// hidden side-effects from the `proxy_port` field.
    #[test]
    fn unsandboxed_runtime_ignores_proxy_port() {
        let policy = SandboxPolicy::default();
        let with = UnsandboxedRuntime
            .transform(SandboxTransformRequest {
                command: "true",
                project_root: Path::new("/tmp"),
                policy: &policy,
                proxy_port: Some(8080),
            })
            .unwrap();
        let without = UnsandboxedRuntime
            .transform(SandboxTransformRequest {
                command: "true",
                project_root: Path::new("/tmp"),
                policy: &policy,
                proxy_port: None,
            })
            .unwrap();
        // Both should be `sh -c true` — no proxy-related divergence in
        // the unsandboxed path. The proxy port is the kernel sandbox's
        // concern, not ours.
        assert_eq!(with.command.as_std().get_program(), "sh");
        assert_eq!(without.command.as_std().get_program(), "sh");
        assert_eq!(
            with.command.as_std().get_args().collect::<Vec<_>>(),
            without.command.as_std().get_args().collect::<Vec<_>>(),
        );
    }

    /// Phase 3c kernel enforcement: when a proxy port is supplied,
    /// macOS Seatbelt must inject the `network_proxied_rules` SBPL
    /// (deny all TCP outbound except `127.0.0.1:proxy_port`). The
    /// profile string is passed via `-p <profile>` so we can grep the
    /// arg vector to confirm.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_runtime_emits_proxied_profile_when_port_set() {
        let policy = SandboxPolicy::strict_default();
        let req = SandboxTransformRequest {
            command: "true",
            project_root: Path::new("/tmp"),
            policy: &policy,
            proxy_port: Some(54321),
        };
        let cmd = SeatbeltRuntime.transform(req).unwrap().command;
        let profile = cmd
            .as_std()
            .get_args()
            .nth(1) // "sandbox-exec" "-p" "<profile>" ...
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        // The proxied profile MUST permit only the proxy port and MUST NOT
        // contain the open-network blanket allow. SBPL requires the host
        // to be `localhost` (not literal `127.0.0.1`) — see
        // [`network_proxied_rules`] for the full rationale (macOS only).
        assert!(
            profile.contains("localhost:54321"),
            "proxied profile must allow loopback proxy port via localhost; got: {profile}"
        );
        assert!(
            !profile.contains("(allow network*)"),
            "proxied profile must NOT include the open-network allow; got: {profile}"
        );
    }

    /// Regression guard: `proxy_port = None` keeps the pre-3c open-network
    /// profile intact. Catches accidental "always-proxied" wiring that
    /// would break callers (sub-agents, scripts) that don't have a proxy.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_runtime_keeps_open_network_when_port_none() {
        let policy = SandboxPolicy::strict_default();
        let req = SandboxTransformRequest {
            command: "true",
            project_root: Path::new("/tmp"),
            policy: &policy,
            proxy_port: None,
        };
        let cmd = SeatbeltRuntime.transform(req).unwrap().command;
        let profile = cmd
            .as_std()
            .get_args()
            .nth(1)
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            profile.contains("(allow network*)"),
            "open-network profile must include the blanket allow; got: {profile}"
        );
        assert!(
            !profile.contains("localhost:") || !profile.contains("network-outbound"),
            "open-network profile must NOT include the proxied loopback outbound rule; got: {profile}"
        );
    }

    /// Phase 3c.1 gap documentation: bwrap accepts `proxy_port = Some`
    /// and produces a working command (no error), even though it can't
    /// kernel-enforce the port restriction. The env-var bouquet from
    /// `koda-core::sandbox::build` does the actual filtering for
    /// well-behaved clients on Linux. Verifies the warn-once doesn't
    /// turn into a panic.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_runtime_accepts_proxy_port_without_error() {
        let policy = SandboxPolicy::strict_default();
        let req = SandboxTransformRequest {
            command: "true",
            project_root: Path::new("/tmp"),
            policy: &policy,
            proxy_port: Some(54321),
        };
        // Construct without spawning. We don't assert anything about the
        // arg vector — bwrap simply doesn't touch the network namespace
        // until 3c.1.
        let _ = BwrapRuntime.transform(req);
    }
}
