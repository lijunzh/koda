//! Per-process resource limits via `setrlimit(2)`.
//!
//! Wires the trust-derived [`crate::policy::ResourceLimits`] into the
//! kernel's `RLIMIT_*` machinery, applied via `pre_exec` so the limits
//! are inherited by the user's command (and any child processes) but
//! never leak into the koda host process itself.
//!
//! ## Threat model fit
//!
//! Backstops a runaway / malicious LLM-issued command. The wall-time
//! ceiling in the shell tool catches the common case (long-running grep
//! that never terminates), but a fork bomb or a memory-allocation loop
//! escapes wall-time \u2014 the koda *host* hangs waiting on a child whose
//! kernel-level resources are unbounded.
//!
//! Three rlimits cover the realistic cases:
//!
//! | Limit | Catches | rlimit |
//! |---|---|---|
//! | CPU seconds | tight infinite loop, busy-wait | `RLIMIT_CPU` (SIGXCPU) |
//! | Address-space bytes | `malloc` blow-up, slab allocator abuse | `RLIMIT_AS` (ENOMEM) |
//! | Open FDs | fork bomb, accept-loop FD exhaustion | `RLIMIT_NOFILE` (EMFILE) |
//!
//! `RLIMIT_RSS` is **not** used: it's a no-op on Linux since 2.4 (the
//! kernel ignores it), so `RLIMIT_AS` is the cross-platform proxy. Its
//! semantics differ slightly (caps virtual memory, not resident set)
//! but the *effect* on a runaway is identical \u2014 oversize allocations
//! fail with `ENOMEM`.
//!
//! ## Why pre_exec
//!
//! `setrlimit` runs in the *child* between `fork` and `execvp`. This
//! has two properties that matter here:
//!
//! 1. The koda host's own rlimits stay untouched. We aren't artificially
//!    capping the orchestrator just because one tool wants a tight cap.
//! 2. The closure must be async-signal-safe \u2014 no allocations, no mutex
//!    acquisitions, only `setrlimit(2)` itself which POSIX guarantees
//!    is safe in this context.
//!
//! ## Platform support
//!
//! Unix-only. Windows builds compile this module into a no-op shim
//! (the spawn path on Windows doesn't go through a sandbox today; #934
//! treats Windows as out-of-scope for the kernel sandbox threat model).

#[cfg(unix)]
use crate::policy::ResourceLimits;
#[cfg(unix)]
use tokio::process::Command;

/// Apply the resource limits in `limits` to `cmd` via a `pre_exec` hook.
///
/// Each `Some(value)` field becomes a `setrlimit` call in the child.
/// `None` fields are skipped \u2014 *not* set to "infinity" \u2014 so an
/// orchestrator that wants to leave a dial alone can do so without
/// having to know what the kernel default is.
///
/// ## Errors
///
/// Failures inside the `pre_exec` closure surface as the spawn returning
/// `io::Error` from `cmd.spawn()`. We intentionally **fail closed**: if
/// the kernel rejects a `setrlimit` (e.g. asking for a higher hard limit
/// than the host has), the child does not exec and the caller sees an
/// error. Better to surface a misconfigured limit at spawn time than to
/// silently run unbounded.
///
/// ## Idempotency
///
/// Calling this multiple times on the same `Command` chains the hooks \u2014
/// every `pre_exec` registered runs in order. The last one's limits win
/// per-resource. Don't call it twice in production; it's allowed only
/// to keep the API uncomplicated.
///
/// ## Safety
///
/// `pre_exec` itself is `unsafe`. The closure we install only calls
/// `setrlimit(2)` which POSIX lists as async-signal-safe, so the inner
/// `unsafe` block carries no additional hazard beyond what `pre_exec`
/// already requires.
#[cfg(unix)]
pub fn apply_to_command(cmd: &mut Command, limits: &ResourceLimits) {
    // Snapshot `Copy` values into the closure so we don't move the
    // borrowed `&ResourceLimits` into a `'static` callback.
    let cpu = limits.cpu_time_secs;
    let rss = limits.max_rss_bytes;
    let fds = limits.max_open_fds;

    // Fast path: nothing to do. Avoid the `unsafe` block + closure
    // allocation entirely when no limits are set (which is the
    // overwhelmingly common case today \u2014 only Auto trust mode populates
    // these).
    if cpu.is_none() && rss.is_none() && fds.is_none() {
        return;
    }

    use std::os::unix::process::CommandExt as _;

    // SAFETY: the closure body only calls `setrlimit(2)`, which POSIX
    // guarantees is async-signal-safe (the constraint that pre_exec
    // imposes on its hook). No allocations, no mutex acquisition, no
    // re-entrant Rust runtime calls.
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            // The `RLIMIT_*` constants have a *different* type on Linux
            // (`__rlimit_resource_t = c_uint`) vs macOS (`c_int`), so a
            // function-typed wrapper would need cfg-per-platform. The
            // macro lets the compiler infer at each call site.
            macro_rules! try_set {
                ($resource:expr, $value:expr) => {
                    if let Some(v) = $value {
                        let limit = libc::rlimit {
                            rlim_cur: v as libc::rlim_t,
                            rlim_max: v as libc::rlim_t,
                        };
                        if libc::setrlimit($resource, &limit) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                };
            }
            try_set!(libc::RLIMIT_CPU, cpu);
            try_set!(libc::RLIMIT_AS, rss);
            try_set!(libc::RLIMIT_NOFILE, fds);
            Ok(())
        });
    }
}

/// Windows shim: rlimits don't exist on this platform; the sandbox
/// threat model treats Windows as out-of-scope (#934). Compiling this
/// as a no-op keeps the call site free of `cfg(unix)` clutter.
#[cfg(not(unix))]
pub fn apply_to_command(
    _cmd: &mut tokio::process::Command,
    _limits: &crate::policy::ResourceLimits,
) {
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::time::timeout;

    /// No-op fast path: a `Default` (all-`None`) ResourceLimits must
    /// not register a `pre_exec` hook \u2014 verified indirectly by the
    /// child running normally and finishing well under any cap.
    #[tokio::test]
    async fn default_limits_are_a_no_op() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo ok");
        apply_to_command(&mut cmd, &ResourceLimits::default());
        let out = cmd.output().await.expect("spawn ok");
        assert!(out.status.success(), "child should succeed: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    }

    /// Load-bearing test for `RLIMIT_NOFILE` \u2014 spawn `sh -c 'ulimit -n'`
    /// with a low FD cap and read it back from stdout. Pins the actual
    /// kernel-observable effect, not just that we called the syscall.
    #[tokio::test]
    async fn nofile_limit_is_observable_in_child() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("ulimit -n");
        apply_to_command(
            &mut cmd,
            &ResourceLimits {
                max_open_fds: Some(64),
                ..Default::default()
            },
        );
        let out = cmd.output().await.expect("spawn ok");
        assert!(out.status.success(), "child should succeed: {out:?}");
        let reported: u64 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ulimit prints a number");
        assert_eq!(reported, 64, "child should see the FD cap we set");
    }

    /// Load-bearing test for `RLIMIT_CPU` \u2014 a tight busy loop hits the
    /// CPU cap and gets killed by `SIGXCPU` (signal 24). We use a
    /// generous 10s wall-time budget to absorb CI noise; on healthy
    /// hardware the kill arrives in < 2s.
    ///
    /// Slow on purpose (waits for the kernel to tick the CPU counter),
    /// but the alternative \u2014 mocking libc \u2014 wouldn't actually prove
    /// the rlimit took effect.
    #[tokio::test]
    async fn cpu_limit_kills_busy_loop() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("while :; do :; done");
        apply_to_command(
            &mut cmd,
            &ResourceLimits {
                cpu_time_secs: Some(1),
                ..Default::default()
            },
        );
        let out = timeout(Duration::from_secs(10), cmd.output())
            .await
            .expect("must finish within wall budget")
            .expect("spawn ok");
        assert!(
            !out.status.success(),
            "busy loop should be killed, got {out:?}"
        );
        // SIGXCPU on Unix is 24; surface via signal exit on Unix.
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGXCPU),
            "child should be killed by SIGXCPU, got status {:?}",
            out.status
        );
    }

    /// Pins the spawn-time fail-closed contract from the rustdoc:
    /// asking for an `RLIMIT_NOFILE` higher than the host's hard cap
    /// must produce an `io::Error` at spawn rather than silently
    /// dropping back to the host limit.
    ///
    /// We pick a value (`u64::MAX`) that no realistic kernel will
    /// permit. If a future host actually allows this, the test would
    /// false-pass \u2014 acceptable, since the spec change would be
    /// "unbounded FDs work" which is exactly what a future kernel
    /// permitting it would mean.
    #[tokio::test]
    async fn unsatisfiable_limit_fails_at_spawn() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo unreachable");
        apply_to_command(
            &mut cmd,
            &ResourceLimits {
                max_open_fds: Some(u64::MAX),
                ..Default::default()
            },
        );
        let result = cmd.output().await;
        assert!(
            result.is_err()
                || !result
                    .as_ref()
                    .map(|o| o.status.success())
                    .unwrap_or(true),
            "unsatisfiable rlimit must fail-closed, got {result:?}"
        );
    }
}
