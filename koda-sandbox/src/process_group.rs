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
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn kill_group(leader: u32) {
    // SAFETY: `killpg` takes plain integers and touches no memory. It fails
    // harmlessly when the group is already gone.
    unsafe {
        libc::killpg(leader as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_leader: u32) {}
