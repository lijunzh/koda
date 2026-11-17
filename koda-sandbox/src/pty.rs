//! A pseudo-terminal for a command the model types into, as Codex's
//! `exec_command` with `tty: true` gets one.
//!
//! The terminal is opened before the sandbox is entered, and the command gets
//! its far end as stdin, stdout and stderr, so programs that check for a
//! terminal (a REPL, a confirmation prompt, `sudo`-style password reads) see
//! one. Koda keeps the near end: what it writes there is typed, and what it
//! reads there is everything the command printed.
//!
//! The command leads a new session, with the terminal as its controlling
//! terminal. That makes it the leader of its own process group too, as
//! [`crate::process_group::isolate`] would, so the group is stopped the same
//! way, and a typed Ctrl+C (`\x03`) reaches the whole group through the
//! terminal, as it does in a real one.

use std::os::fd::OwnedFd;
use tokio::process::Command;

/// The terminal's size: the classic 80 by 24, which every program handles.
const COLUMNS: u16 = 80;
const ROWS: u16 = 24;

/// A new terminal: Koda's end (`controller`) and the command's (`terminal`).
pub struct Pty {
    /// What Koda reads the output from and writes the typing to.
    pub controller: OwnedFd,
    /// The command's stdin, stdout and stderr.
    pub terminal: OwnedFd,
}

/// Open a new pseudo-terminal.
#[cfg(unix)]
pub fn open() -> std::io::Result<Pty> {
    use std::os::fd::FromRawFd as _;
    let mut controller: libc::c_int = -1;
    let mut terminal: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: ROWS,
        ws_col: COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: both out-pointers are valid; a null name and termios are allowed.
    // `mut` pointers fit both macOS's signature and Linux's `const` one.
    let rc = unsafe {
        libc::openpty(
            &mut controller,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both are open descriptors we now own.
    let (controller, terminal) = unsafe {
        (
            OwnedFd::from_raw_fd(controller),
            OwnedFd::from_raw_fd(terminal),
        )
    };
    for fd in [&controller, &terminal] {
        set_cloexec(fd)?;
    }
    Ok(Pty {
        controller,
        terminal,
    })
}

/// The device path of the command's end (`/dev/ttys012`, `/dev/pts/3`), for
/// the sandbox to open to it alone.
#[cfg(unix)]
pub fn device_path(terminal: &OwnedFd) -> std::io::Result<std::path::PathBuf> {
    use std::os::fd::AsRawFd as _;
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: the buffer and its length match; the fd is open.
    let rc = unsafe { libc::ttyname_r(terminal.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    // SAFETY: ttyname_r wrote a NUL-terminated string into the buffer.
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    Ok(std::path::PathBuf::from(
        name.to_string_lossy().into_owned(),
    ))
}

/// Windows shim: the sandbox treats Windows as out of scope.
#[cfg(not(unix))]
pub fn device_path(_terminal: &OwnedFd) -> std::io::Result<std::path::PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a terminal for a command needs Unix",
    ))
}

/// Windows shim: the sandbox treats Windows as out of scope.
#[cfg(not(unix))]
pub fn open() -> std::io::Result<Pty> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a terminal for a command needs Unix",
    ))
}

#[cfg(unix)]
fn set_cloexec(fd: &OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: fcntl on a descriptor we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Give `cmd` the terminal as stdin, stdout and stderr, and make it the
/// terminal's session leader. Use this instead of
/// [`crate::process_group::isolate`]: a group leader can't start a session.
#[cfg(unix)]
pub fn attach(cmd: &mut Command, terminal: &OwnedFd) -> std::io::Result<()> {
    let stdio = || terminal.try_clone().map(std::process::Stdio::from);
    cmd.stdin(stdio()?).stdout(stdio()?).stderr(stdio()?);
    // SAFETY: setsid and ioctl are async-signal-safe, and the closure
    // allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // stdin is the terminal by now: make it the controlling one, so a
            // typed Ctrl+C becomes SIGINT for the group.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Windows shim, as [`open`] is.
#[cfg(not(unix))]
pub fn attach(_cmd: &mut Command, _terminal: &OwnedFd) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "a terminal for a command needs Unix",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Read from `controller` until `want` shows up or the command is done.
    fn read_until(controller: &mut std::fs::File, want: &str) -> String {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !String::from_utf8_lossy(&seen).contains(want) {
            match controller.read(&mut buf) {
                Ok(0) | Err(_) => break, // EIO once the command's end is closed
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[tokio::test]
    async fn the_command_sees_a_terminal_and_reads_what_is_typed() {
        let pty = open().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("[ -t 0 ] && echo IS-A-TTY; read answer; echo \"got $answer\"");
        attach(&mut cmd, &pty.terminal).unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        drop(pty.terminal);
        let mut controller = std::fs::File::from(pty.controller);
        assert!(read_until(&mut controller, "IS-A-TTY").contains("IS-A-TTY"));
        controller.write_all(b"yes\n").unwrap();
        assert!(read_until(&mut controller, "got yes").contains("got yes"));
        assert!(child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn a_typed_ctrl_c_interrupts_the_command() {
        let pty = open().unwrap();
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("echo READY; sleep 30");
        attach(&mut cmd, &pty.terminal).unwrap();
        let mut child = cmd.spawn().unwrap();
        drop(cmd);
        drop(pty.terminal);
        let mut controller = std::fs::File::from(pty.controller);
        read_until(&mut controller, "READY");
        controller.write_all(b"\x03").unwrap();
        // Keep reading: a command can't finish exiting while the terminal
        // holds output nobody read (here, the echoed `^C`).
        let reader = std::thread::spawn(move || read_until(&mut controller, "never printed"));
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .expect("Ctrl+C should stop it")
            .unwrap();
        assert!(!status.success());
        assert!(reader.join().unwrap().contains("^C"));
    }
}
