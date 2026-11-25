//! ugrep and bfs inside the `koda` binary, as Claude Code builds them into
//! its own.
//!
//! Claude Code's shell gives the model `grep` and `find` functions that run
//! its own executable under the name `ugrep` or `bfs`; the executable sees
//! that name and becomes that program. [`dispatch`] does the same for
//! `koda`: started as `ugrep` or `bfs` (a link of that name, or
//! `exec -a`), it runs that program and exits. The programs are ugrep 7.8.5
//! and bfs 4.1.7, built with the features Claude Code's have (see
//! `build.rs`); their sources are vendored unchanged under `vendor/`, with
//! their licences.

// The compression libraries ugrep's `-z` calls into, linked from their
// `-sys` crates.
use bzip2_sys as _;
use libz_sys as _;
use zstd_sys as _;

use std::ffi::{CString, OsStr, c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

unsafe extern "C" {
    fn koda_search_ugrep(argc: c_int, argv: *const *const c_char) -> c_int;
    fn koda_search_bfs(argc: c_int, argv: *mut *mut c_char) -> c_int;
}

/// A program built into this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Ugrep,
    Bfs,
}

impl Program {
    /// Every program built in.
    pub const ALL: [Program; 2] = [Program::Ugrep, Program::Bfs];

    /// The name the program answers to.
    pub fn name(self) -> &'static str {
        match self {
            Program::Ugrep => "ugrep",
            Program::Bfs => "bfs",
        }
    }

    /// The program `argv0` names: its last path component, exactly
    /// `ugrep` or `bfs`.
    pub fn from_argv0(argv0: &OsStr) -> Option<Program> {
        let name = Path::new(argv0).file_name()?;
        Program::ALL.into_iter().find(|p| name == p.name())
    }
}

/// If this process was started as `ugrep` or `bfs`, run that program with
/// the process's arguments and exit with its status; otherwise return.
///
/// Call it first in `main`, before any thread starts.
pub fn dispatch() {
    let mut args = std::env::args_os();
    let Some(program) = args.next().as_deref().and_then(Program::from_argv0) else {
        return;
    };
    let argv: Vec<CString> = std::env::args_os()
        .map(|a| CString::new(a.as_bytes()).unwrap_or_default())
        .collect();
    std::process::exit(run(program, &argv));
}

/// Run `program` with `argv` (`argv[0]` included) in this process and
/// return its exit status. The program may also exit the process itself.
pub fn run(program: Program, argv: &[CString]) -> i32 {
    // Rust starts every program with SIGPIPE ignored. A grep or find whose
    // reader has gone (`grep … | head`) should die of it, as the system's
    // do, and pass the default on to what `find -exec` runs.
    // SAFETY: resetting a signal to its default before any thread starts.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut ptrs: Vec<*mut c_char> = argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    let argc = c_int::try_from(ptrs.len()).unwrap_or(c_int::MAX);
    ptrs.push(std::ptr::null_mut());
    // SAFETY: `ptrs` is a NULL-terminated array of NUL-terminated strings
    // that outlive the call, as C's `main` expects.
    unsafe {
        match program {
            Program::Ugrep => koda_search_ugrep(argc, ptrs.as_ptr().cast()),
            Program::Bfs => koda_search_bfs(argc, ptrs.as_mut_ptr()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_named_by_their_last_path_component() {
        assert_eq!(
            Program::from_argv0(OsStr::new("ugrep")),
            Some(Program::Ugrep)
        );
        assert_eq!(
            Program::from_argv0(OsStr::new("/a/b/bfs")),
            Some(Program::Bfs)
        );
        assert_eq!(Program::from_argv0(OsStr::new("koda")), None);
        assert_eq!(Program::from_argv0(OsStr::new("/x/ugrep-old")), None);
        assert_eq!(Program::from_argv0(OsStr::new("grep")), None);
    }
}
