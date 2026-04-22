//! Stage 2 helper: in-sandbox setup that runs after `bwrap --unshare-net`
//! cuts host networking, before the user command starts (Phase 3c.1.c
//! of #934).
//!
//! ## Where this runs in the lifecycle
//!
//! ```text
//! koda agent (host, tokio)
//!   └─> BuiltInProxy::spawn  — binds host TCP + UDS bridge          ┌─── PHASE 3c.1.b
//!   └─> BwrapRuntime::transform                                      │
//!         └─> Command::new("bwrap")                                  │
//!               │    --ro-bind / / --bind /tmp /tmp                    │ PHASE 3c.1.d
//!               │    --unshare-net --unshare-user --unshare-pid        │ (next commit)
//!               │    --bind <uds> <uds>                                │
//!               │    -- /path/to/koda-sandbox-stage2 sh -c "$user_cmd" ┘
//!               └──> [INSIDE THE NETNS]
//!                    koda-sandbox-stage2 (THIS MODULE)            ┌─── THIS COMMIT
//!                    ├─> bring up `lo` (ioctl SIOCSIFFLAGS)        │
//!                    ├─> bind TCP listener on 127.0.0.1:0          │
//!                    ├─> fork child: TCP↔UDS bridge thread         │
//!                    ├─> rewrite HTTPS_PROXY etc to new TCP port   │
//!                    └─> execvp into ["sh", "-c", user_cmd]         ┘
//!                          └─> user code runs sandboxed, talks to
//!                                127.0.0.1:NEW_PORT  →  bridge child
//!                                →  UDS  →  host TCP  →  proxy filter
//! ```
//!
//! ## Why no tokio
//!
//! Stage 2 runs in every sandboxed shell invocation (every Bash tool
//! call). Pulling in a tokio runtime would add ~5 ms of startup latency
//! and ~3 MB to the binary. Pure std + libc keeps the helper tiny and
//! fast — the bridge is just two `std::thread::spawn` blocking copies.
//!
//! ## Why fork before execvp
//!
//! The TCP listener thread must outlive `execvp` (which replaces the
//! current process image with the user command). std::thread doesn't
//! survive `execvp` — the kernel kills all threads except the calling
//! one. So we `fork()`: the child stays alive running the bridge loop;
//! the parent execs the user command. Parent-death-signal
//! (`PR_SET_PDEATHSIG`) ensures the bridge child dies when the user
//! command exits, even if the user command doesn't reap it.
//!
//! ## Failure modes
//!
//! Every failure here is fatal to the sandboxed command — if we can't
//! set up the bridge, the user's command will hit a dead `HTTPS_PROXY`
//! and timeout. We exit with a distinctive code (88) so callers can
//! distinguish "sandbox setup failed" from "user command failed".

#![cfg(target_os = "linux")]

use crate::bwrap_proxy::{STAGE2_REWRITE_KEYS_ENV_KEY, STAGE2_UDS_ENV_KEY, rewrite_proxy_url_port};
use anyhow::{Context, Result, bail};
use std::ffi::{CString, OsString};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// Exit code used when stage 2 fails before reaching execvp.
///
/// 88 is arbitrary but distinct from the standard 0/1/2/126/127 plus
/// the 64–78 sysexits range. Lets callers distinguish "sandbox setup
/// failed" from "user command failed/not found".
pub const STAGE2_SETUP_FAILED_EXIT: i32 = 88;

/// Stage 2 entry point. Called from `bin/koda-sandbox-stage2.rs`.
///
/// `argv` is the full argument vector the helper was invoked with;
/// `argv[0]` is the helper binary path (we don't use it), `argv[1..]`
/// is the user command we'll execvp into.
///
/// Never returns on success — it execs the user command. Returns
/// `Err` only if setup fails before exec.
pub fn run(argv: Vec<OsString>) -> Result<()> {
    if argv.len() < 2 {
        bail!("stage2: missing user command (usage: koda-sandbox-stage2 <cmd> [args...])");
    }

    let uds_path = std::env::var(STAGE2_UDS_ENV_KEY)
        .with_context(|| format!("stage2: {STAGE2_UDS_ENV_KEY} env var missing"))?;
    let uds_path = PathBuf::from(uds_path);
    if !uds_path.exists() {
        bail!(
            "stage2: UDS bridge socket {} not found inside sandbox (was it bind-mounted with --bind?)",
            uds_path.display()
        );
    }

    // Step 1: bring `lo` up. Required because --unshare-net leaves
    // the new netns's loopback interface DOWN by default.
    bring_up_loopback().context("stage2: bring up lo")?;

    // Step 2: bind a TCP listener on the in-netns loopback. Port 0 ->
    // kernel-assigned ephemeral port. We must read the port back
    // before forking so the parent can rewrite env vars correctly.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("stage2: bind in-netns TCP listener")?;
    let local_port = listener
        .local_addr()
        .context("stage2: read in-netns listener port")?
        .port();

    // Step 3: fork a bridge child that owns the listener. Parent
    // continues to env rewriting + execvp. Child loops accept-and-pipe
    // until killed (by parent-death signal, since the user command
    // becomes its parent after execvp — same PID).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("stage2: fork failed: {}", std::io::Error::last_os_error());
    }
    if pid == 0 {
        // Child: become the bridge.
        run_bridge_child(listener, uds_path);
        // Unreachable in normal flow; if it returns we exit cleanly.
        unsafe { libc::_exit(0) };
    }

    // Step 4: rewrite proxy env vars. The host-side values point at
    // 127.0.0.1:HOST_PORT which is unreachable in this netns; we want
    // them to point at our new in-netns listener.
    let rewrite_keys = std::env::var(STAGE2_REWRITE_KEYS_ENV_KEY).unwrap_or_default();
    for key in rewrite_keys.split(',').filter(|k| !k.is_empty()) {
        let Ok(old) = std::env::var(key) else {
            continue;
        };
        let Some(new) = rewrite_proxy_url_port(&old, local_port) else {
            // Pass-through if the value isn't a parseable URL —
            // user-supplied weirdness, not our problem here.
            continue;
        };
        // SAFETY: stage 2 is single-threaded at this point (the
        // bridge child is in a separate process via fork, not a
        // thread). std::env::set_var is sound when no other thread
        // is reading env.
        unsafe { std::env::set_var(key, new) };
    }

    // Clean up the stage 2 marker env vars so user commands don't
    // see (and potentially leak) our internal contract.
    unsafe {
        std::env::remove_var(STAGE2_UDS_ENV_KEY);
        std::env::remove_var(STAGE2_REWRITE_KEYS_ENV_KEY);
    }

    // Step 5: execvp into the user command. argv[1] is the program;
    // argv[1..] becomes the new argv.
    let prog = argv[1].clone();
    let prog_c = osstring_to_cstring(&prog).context("stage2: convert program name")?;
    let arg_cs: Vec<CString> = argv[1..]
        .iter()
        .map(osstring_to_cstring)
        .collect::<Result<Vec<_>>>()
        .context("stage2: convert command args")?;
    let arg_ptrs: Vec<*const libc::c_char> = arg_cs
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    // SAFETY: pointers in `arg_ptrs` are kept alive by `arg_cs` for
    // the entire syscall duration; null terminator present.
    unsafe { libc::execvp(prog_c.as_ptr(), arg_ptrs.as_ptr()) };

    // Only reached if execvp failed.
    let err = std::io::Error::last_os_error();
    bail!("stage2: execvp({:?}) failed: {err}", prog);
}

/// Convert an `OsString` to a `CString` for syscall use.
fn osstring_to_cstring(s: &OsString) -> Result<CString> {
    CString::new(s.clone().into_vec())
        .with_context(|| format!("stage2: argv contains NUL byte: {s:?}"))
}

/// Bring the loopback (`lo`) interface UP via SIOCSIFFLAGS ioctl.
///
/// Required because `bwrap --unshare-net` creates a fresh net
/// namespace whose loopback exists but starts DOWN — attempting to
/// bind 127.0.0.1 fails with EADDRNOTAVAIL until `lo` is up.
///
/// Equivalent to `ip link set lo up` but doesn't require the `ip`
/// binary to be present in the sandbox.
fn bring_up_loopback() -> Result<()> {
    // SOCK_DGRAM is fine for ioctl-on-interface — the socket is just a
    // handle, never used to send packets.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        bail!("socket(AF_INET): {}", std::io::Error::last_os_error());
    }
    // Defer fd close.
    let _close = CloseOnDrop(fd);

    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    // "lo\0" — ifreq.ifr_name is a fixed-size i8 array.
    let name: &[u8] = b"lo\0";
    for (i, b) in name.iter().enumerate() {
        req.ifr_name[i] = *b as libc::c_char;
    }

    // Read current flags.
    let r = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut req) };
    if r < 0 {
        bail!(
            "ioctl(SIOCGIFFLAGS, lo): {}",
            std::io::Error::last_os_error()
        );
    }
    // Set IFF_UP (and IFF_RUNNING for good measure — some kernels
    // require it for loopback to actually carry traffic).
    let up = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    let cur = unsafe { req.ifr_ifru.ifru_flags };
    if (cur & up) == up {
        return Ok(()); // already up
    }
    // Write to a Copy union field is safe in current Rust (only
    // reads need unsafe, since the discriminant isn't tracked).
    // Wrapping this in `unsafe { }` triggers `unused_unsafe` under
    // CI's -D warnings.
    req.ifr_ifru.ifru_flags = cur | up;
    let r = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &req) };
    if r < 0 {
        bail!(
            "ioctl(SIOCSIFFLAGS, lo|UP): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// RAII guard that closes a fd on drop.
struct CloseOnDrop(libc::c_int);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Body of the bridge child process. Runs forever (until killed by
/// parent-death signal) accepting TCP connections and forwarding each
/// to a Unix socket connection.
///
/// One blocking thread per accepted connection — fine because the
/// concurrent connection count is bounded by what the user's command
/// produces, and these threads spend their lives in `read`/`write`
/// syscalls (cheap).
fn run_bridge_child(listener: TcpListener, uds_path: PathBuf) {
    // PR_SET_PDEATHSIG: when our parent (which becomes the user
    // command after execvp — same PID) exits, send us SIGTERM. Without
    // this, a forked bridge would orphan into init and survive
    // forever.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) };

    // Race window: the parent may have already exited between fork
    // and prctl. Check explicitly.
    if unsafe { libc::getppid() } == 1 {
        return;
    }

    loop {
        let (tcp, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return, // listener closed
        };
        let path = uds_path.clone();
        std::thread::spawn(move || {
            let uds = match UnixStream::connect(&path) {
                Ok(s) => s,
                Err(_) => return,
            };
            let _ = bridge_two_streams(tcp, uds);
        });
    }
}

/// Bidirectional copy between a TCP and a Unix stream. Returns when
/// either direction EOFs.
fn bridge_two_streams(tcp: TcpStream, uds: UnixStream) -> std::io::Result<()> {
    let tcp_r = tcp.try_clone()?;
    let mut tcp_w = tcp;
    let uds_r = uds.try_clone()?;
    let mut uds_w = uds;

    let h = std::thread::spawn(move || copy_until_eof(tcp_r, &mut uds_w));
    let _ = copy_until_eof(uds_r, &mut tcp_w);
    let _ = h.join();
    Ok(())
}

/// `std::io::copy` with a fixed buffer; tolerates `Interrupted` (EINTR).
fn copy_until_eof<R: Read, W: Write>(mut r: R, w: &mut W) -> std::io::Result<u64> {
    let mut buf = [0u8; 8 * 1024];
    let mut total = 0u64;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osstring_to_cstring_rejects_nul() {
        let s = OsString::from_vec(b"foo\0bar".to_vec());
        assert!(osstring_to_cstring(&s).is_err());
    }

    #[test]
    fn osstring_to_cstring_accepts_normal_strings() {
        let s = OsString::from("sh");
        let c = osstring_to_cstring(&s).unwrap();
        assert_eq!(c.to_bytes(), b"sh");
    }

    #[test]
    fn copy_until_eof_handles_simple_payload() {
        // Self-contained pipe roundtrip — no syscalls beyond memory.
        let mut input = std::io::Cursor::new(b"hello world".to_vec());
        let mut output: Vec<u8> = Vec::new();
        let n = copy_until_eof(&mut input, &mut output).unwrap();
        assert_eq!(n, 11);
        assert_eq!(&output, b"hello world");
    }

    #[test]
    fn stage2_setup_failed_exit_is_88() {
        // Locked-in API: callers (notably the e2e test in 3c.1.e)
        // assert on this exit code to distinguish setup failure from
        // user-command failure. Bumping it is a breaking change.
        assert_eq!(STAGE2_SETUP_FAILED_EXIT, 88);
    }
}
