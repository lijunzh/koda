//! ugrep and bfs inside the `koda` binary, as Claude Code builds them into
//! its own.
//!
//! Claude Code's shell gives the model `grep` and `find` functions that run
//! its own executable under the name `ugrep` or `bfs`; the executable sees
//! that name and becomes that program. [`dispatch`] does the same for
//! `koda`, which runs a program when it is started under its name (a link
//! named `ugrep` or `bfs`) or with its flag as the first argument
//! ([`Program::flag`], as Codex's binary runs `apply_patch` when started
//! with `--codex-run-as-apply-patch`). The flag works from any shell, dash
//! included, which has no `exec -a`. The programs are ugrep 7.8.5 and bfs
//! 4.1.7, built with the features Claude Code's have (see `build.rs`);
//! their sources are vendored unchanged under `vendor/`, with their
//! licences.

// The compression libraries ugrep's `-z` calls into, linked from their
// `-sys` crates.
use bzip2_sys as _;
use libz_sys as _;
use zstd_sys as _;

use std::ffi::{CString, OsStr, c_char, c_int};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

    /// The first argument that makes `koda` run this program.
    pub fn flag(self) -> &'static str {
        match self {
            Program::Ugrep => "--koda-run-as-ugrep",
            Program::Bfs => "--koda-run-as-bfs",
        }
    }

    /// The program `argv0` names: its last path component, exactly
    /// `ugrep` or `bfs`.
    pub fn from_argv0(argv0: &OsStr) -> Option<Program> {
        let name = Path::new(argv0).file_name()?;
        Program::ALL.into_iter().find(|p| name == p.name())
    }
}

/// The executable [`dispatch`] recorded: the running `koda`, which runs
/// the programs when given their [`Program::flag`]. `None` in a process
/// whose `main` didn't call [`dispatch`], such as a test binary, which
/// has no programs to run.
static EXE: OnceLock<PathBuf> = OnceLock::new();

/// The executable that runs the programs, if this process is one
/// ([`dispatch`] was called and found its own path).
pub fn exe() -> Option<&'static Path> {
    EXE.get().map(PathBuf::as_path)
}

/// If this process was started as `ugrep` or `bfs`, or with one's flag as
/// its first argument, run that program with the remaining arguments and
/// exit with its status. Otherwise record this executable for [`exe`] and
/// return.
///
/// Call it first in `main`, before any thread starts.
pub fn dispatch() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(argv0) = args.first() else { return };
    let to_c = |a: &std::ffi::OsString| CString::new(a.as_bytes()).unwrap_or_default();
    if let Some(program) = Program::from_argv0(argv0) {
        let argv: Vec<CString> = args.iter().map(to_c).collect();
        std::process::exit(run(program, &argv));
    }
    if let Some(program) = args
        .get(1)
        .and_then(|a| Program::ALL.into_iter().find(|p| a == p.flag()))
    {
        // The program sees its own name as `argv[0]`, as under Claude
        // Code's `exec -a`.
        let argv: Vec<CString> = std::iter::once(CString::new(program.name()).unwrap())
            .chain(args[2..].iter().map(to_c))
            .collect();
        std::process::exit(run(program, &argv));
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = EXE.set(exe);
    }
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
