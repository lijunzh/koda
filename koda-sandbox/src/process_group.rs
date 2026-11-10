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
#[cfg(unix)]
pub fn kill_group(leader: u32) {
    // SAFETY: `killpg` takes plain integers and touches no memory. It fails
    // harmlessly when the group is already gone.
    unsafe {
        libc::killpg(leader as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub fn kill_group(_leader: u32) {}
