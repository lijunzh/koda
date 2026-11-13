//! Stopping a command together with everything it started.
//!
//! Killing the child a caller spawned isn't enough. Under `bwrap` that child
//! is bwrap, whose own fork goes on running the command, and a shell's forked
//! jobs outlive the shell on every platform. A process group reaches them all.
//! bwrap stays in the group, since it's run without `--new-session`.
//!
//! ## What a group of its own costs
//!
//! Signals sent to Koda's group no longer reach the command, so a terminal's
//! Ctrl+C or hangup doesn't either. Whoever holds the [`ProcessGroup`] has to
//! stop it. A background group also stops if it reads the terminal, so give
//! the command a stdin that isn't one.
//!
//! A command that leaves the group on purpose (`setsid`, `set -m`) isn't
//! reached.
//!
//! ## A group outlives its leader
//!
//! A shell that runs `server &` and exits leaves the server in its group. As
//! in Codex, whose exec sessions own their process group, Koda treats the
//! group as the command's while anything in it is alive ([`group_alive`]),
//! and any process in it as the command's ([`group_of`]).

use tokio::process::{Child, Command};

/// Start `cmd` as the leader of a new process group.
#[cfg(unix)]
pub fn isolate(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    cmd.as_std_mut().process_group(0);
}

/// Windows shim, as in [`crate::rlimits`]: there are no process groups to use,
/// and the sandbox treats Windows as out of scope.
#[cfg(not(unix))]
pub fn isolate(_cmd: &mut Command) {}

/// The group of a child started under [`isolate`]. Dropping this kills it, so
/// a call that's cut off takes its command with it.
///
/// Hold it in a binding declared after the [`Child`]. The child then outlives
/// it, and the group's id can't be reused before the kill.
#[derive(Debug)]
pub struct ProcessGroup {
    leader: Option<u32>,
}

impl ProcessGroup {
    /// The group `child` leads.
    pub fn of(child: &Child) -> Self {
        Self { leader: child.id() }
    }

    /// Kill every process in the group. Only the first call does anything.
    pub fn kill(&mut self) {
        if let Some(leader) = self.leader.take() {
            kill_group(leader);
        }
    }

    /// The command finished. Whatever it left running stays running, and the
    /// id is no longer ours to signal.
    pub fn release(&mut self) {
        self.leader = None;
    }

    /// Hand the group over without killing it: the leader's id, for a caller
    /// that takes the group on (a registry of background commands).
    pub fn take(&mut self) -> Option<u32> {
        self.leader.take()
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Whether any process is left in the group `leader` led, the leader itself
/// included. A leader that has exited but not been reaped still counts, so
/// reap it first.
#[cfg(unix)]
pub fn group_alive(leader: u32) -> bool {
    // SAFETY: signal 0 only checks that the group exists and may be signalled.
    unsafe { libc::killpg(leader as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn group_alive(_leader: u32) -> bool {
    false
}

/// The process group `pid` is in, if it's alive.
#[cfg(unix)]
pub fn group_of(pid: u32) -> Option<u32> {
    // SAFETY: plain integer in and out; fails with -1 for a gone process.
    let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
    (pgid > 0).then_some(pgid as u32)
}

#[cfg(not(unix))]
pub fn group_of(_pid: u32) -> Option<u32> {
    None
}

/// Whether the process `pid` is alive.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    false
}

/// SIGKILL every process in the group `leader` led. Harmless when it's gone.
///
/// A process caught by the kill in the middle of a fork can leave the new
/// child alive in the group: Linux restarts such a fork so the child gets the
/// signal too, but macOS doesn't (`bash -c 'sleep 71 & sleep 72; wait'`
/// killed just as it started left a `sleep` behind 24 times in 400). So when
/// the leader is our child, the group is killed again once the leader has
/// died. Anything the leader forked is in the group by then, and the leader,
/// dead but not yet reaped, holds its id, so the group is still the one we
/// meant. A leader that is already reaped, or isn't ours, gets no second kill.
/// Linux needs none, so there it's skipped.
#[cfg(unix)]
pub fn kill_group(leader: u32) {
    // SAFETY: `killpg` takes plain integers and touches no memory. It fails
    // harmlessly when the group is already gone.
    unsafe {
        libc::killpg(leader as libc::pid_t, libc::SIGKILL);
    }
    if cfg!(not(target_os = "linux")) && leader_died_unreaped(leader) {
        // SAFETY: as above.
        unsafe {
            libc::killpg(leader as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Whether `leader`, a child of ours just sent SIGKILL, has died without
/// being reaped, waiting up to 100 ms for it to. `WNOWAIT` leaves it for
/// whoever reaps it, such as a [`Child`] that waits on it later.
#[cfg(unix)]
fn leader_died_unreaped(leader: u32) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value of the plain C struct.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable `siginfo_t`; the other arguments
        // are plain integers. `WNOWAIT` leaves the child unreaped.
        let found = unsafe {
            libc::waitid(
                libc::P_PID,
                leader as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if found != 0 {
            // ECHILD: reaped already, or not our child. Its id may be reused.
            return false;
        }
        // With WNOHANG, a child still running leaves `info` as it was: zero.
        if info.si_signo == libc::SIGCHLD {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(not(unix))]
pub fn kill_group(_leader: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;

    fn sleeps_left() -> Vec<String> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "^sleep 8131[12]$"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// Killed while the shell is forking its jobs, the group leaves none
    /// behind. Before the second kill, macOS kept a `sleep` alive about one
    /// time in sixteen; Linux never does.
    #[test]
    fn a_group_killed_mid_fork_leaves_nothing() {
        // 81311 and 81312 s: no other test sleeps that long, so pgrep finds only these.
        for i in 0..200u64 {
            let mut child = std::process::Command::new("bash")
                .args(["-c", "sleep 81311 & sleep 81312; wait"])
                .process_group(0)
                .spawn()
                .unwrap();
            // From at once to 4 ms in: across the shell's two forks.
            std::thread::sleep(std::time::Duration::from_micros(i % 9 * 500));
            kill_group(child.id());
            child.wait().unwrap();
        }
        let left = sleeps_left();
        for pid in &left {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }
        assert!(left.is_empty(), "{} left behind: {left:?}", left.len());
    }
}
