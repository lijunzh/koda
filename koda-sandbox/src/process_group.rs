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
    /// A group that wouldn't die is logged by [`kill_group`].
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

/// How long [`kill_group`] goes on killing a group whose processes are still
/// running. Codex waits as long for a group it killed
/// (`wait_for_process_group_exit` in its `utils/pty`, a second where its MCP
/// launcher calls it). A killed process is gone as soon as the kernel runs it
/// again, so this only runs out on a machine too busy to.
#[cfg(target_os = "macos")]
const KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// SIGKILL every process in the group `leader` led, and say whether that
/// settled it: `true` when nothing in the group can run again, `false` when
/// something was still running after a second (logged, too). Harmless, and
/// `true`, when the group is already gone.
///
/// One kill isn't always enough. A process caught by it in the middle of a
/// fork can leave the new child alive in the group: Linux restarts such a
/// fork so the child gets the signal too, but macOS doesn't (`bash -c 'sleep
/// 71 & sleep 72; wait'` killed just as it started left a `sleep` behind 24
/// times in 400). So on macOS the group is killed again, a millisecond apart,
/// until nothing in it is running: whatever a dying process forked is in the
/// group by then, and has been killed. The processes still running hold the
/// group's id, so each kill reaches the group that was meant, whoever leads
/// it and whether or not the leader has been reaped.
///
/// It used to kill a second time once the leader had died, which it could
/// only see in a child of its own, and waited 100 ms for: a busy machine, or
/// a group led by someone else's child (one a finished command left its jobs
/// in), got one kill and could keep a job.
#[cfg(target_os = "macos")]
pub fn kill_group(leader: u32) -> bool {
    let deadline = std::time::Instant::now() + KILL_WAIT;
    // Nothing was seen running after the last kill.
    let mut quiet = false;
    loop {
        // SAFETY: `killpg` takes plain integers and touches no memory. It
        // fails harmlessly when the group is already gone.
        unsafe { libc::killpg(leader as libc::pid_t, libc::SIGKILL) };
        if may_be_running(leader) {
            quiet = false;
        } else if quiet {
            return true;
        } else {
            // Quiet once: kill and look once more. The list can miss a
            // child that a process forked as it died; the kill doesn't.
            quiet = true;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                "process group {leader} still has running processes {KILL_WAIT:?} after its kill"
            );
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// SIGKILL every process in the group `leader` led. Always `true`: Linux
/// delivers a group's kill to a process being forked too, so nothing in the
/// group can run again once `killpg` returns, though the processes may not
/// have died yet ([`group_alive`] says when the last one has). Koda runs on
/// nothing else but macOS, where one kill isn't enough.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn kill_group(leader: u32) -> bool {
    // SAFETY: `killpg` takes plain integers and touches no memory. It fails
    // harmlessly when the group is already gone.
    unsafe { libc::killpg(leader as libc::pid_t, libc::SIGKILL) };
    true
}

/// Whether a process of `group` may still be running after a kill, so that
/// it, or something it forks, may be one the kill missed.
///
/// macOS lists a group's dead-but-unreaped processes with its running ones,
/// and answers "no such process" for the details of a dead one and of one
/// that is exiting: taking that answer for "dead" let a job through on a
/// busy machine. So only two things count as not running: off the list, and
/// the leader when it is our own child and has died (it stays listed until
/// its `Child` is waited on).
#[cfg(target_os = "macos")]
fn may_be_running(group: u32) -> bool {
    let mut pids = [0 as libc::pid_t; 256];
    // SAFETY: `pids` is a writable buffer of the size given, in bytes.
    let listed = unsafe {
        libc::proc_listpgrppids(
            group as libc::pid_t,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        )
    };
    let Ok(listed) = usize::try_from(listed) else {
        // Can't list them: go by whether the group can still be signalled.
        return group_alive(group);
    };
    pids.iter()
        .take(listed)
        .any(|pid| !(*pid == group as libc::pid_t && died_unreaped(group)))
}

/// Whether `leader` is a child of ours that has died and not been reaped.
/// `WNOWAIT` leaves it for whoever reaps it, such as a [`Child`] that waits
/// on it later.
#[cfg(target_os = "macos")]
fn died_unreaped(leader: u32) -> bool {
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
    // ECHILD: reaped already, or not our child. With WNOHANG, a child still
    // running leaves `info` as it was: zero.
    found == 0 && info.si_signo == libc::SIGCHLD
}

/// No process groups here (see [`isolate`]), so nothing is left in one.
#[cfg(not(unix))]
pub fn kill_group(_leader: u32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;

    /// The PIDs of the processes whose command line matches `pattern`.
    fn running(pattern: &str) -> Vec<String> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// Fail if a process matching `pattern` is still running, after killing it.
    fn assert_none_left(pattern: &str) {
        // A killed `sleep` is gone once the kernel has got to it, which a
        // busy runner can take a moment over: wait up to 10 s for the last
        // of them. One still there then was left behind.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut left = running(pattern);
        while !left.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            left = running(pattern);
        }
        for pid in &left {
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
        }
        assert!(left.is_empty(), "{} left behind: {left:?}", left.len());
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
            assert!(kill_group(child.id()), "the group was still running");
            child.wait().unwrap();
        }
        assert_none_left("^sleep 8131[12]$");
    }

    /// The same for a group whose leader isn't a child of ours, as the group
    /// a finished command left its jobs in no longer is. The kill once
    /// waited for the leader to die before killing again, which it can only
    /// see in a child of its own (and only for 100 ms), so such a group got
    /// one kill, and on macOS a job forked during it lived on.
    #[test]
    fn a_group_led_by_another_s_child_killed_mid_fork_leaves_nothing() {
        use std::io::BufRead as _;
        // 81321 and 81322 s: no other test sleeps that long, so pgrep finds only these.
        for i in 0..600u64 {
            // `set -m` gives the job a group of its own, led by the inner
            // shell: the outer shell's child, not ours. `$!` is that group.
            let mut outer = std::process::Command::new("bash")
                .args([
                    "-c",
                    "set -m; bash -c 'sleep 81321 & sleep 81322; wait' & echo $!; wait",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut line = String::new();
            std::io::BufReader::new(outer.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            let leader: u32 = line.trim().parse().expect("the job's process group");
            // From at once to 6 ms in: across the inner shell's start and
            // its two forks.
            std::thread::sleep(std::time::Duration::from_micros(i % 25 * 250));
            assert!(kill_group(leader), "the group was still running");
            outer.wait().unwrap();
        }
        assert_none_left("^sleep 8132[12]$");
    }
}
