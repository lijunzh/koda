//! `koda` started as `ugrep` or `bfs` is that program, as Claude Code's
//! executable is (koda-search). These run the real binary through a link
//! of each name, with the arguments Claude Code's `grep` and `find` shell
//! functions pass.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A folder with links named `ugrep` and `bfs` to the built `koda`.
fn links() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let koda = Path::new(env!("CARGO_BIN_EXE_koda"));
    let ugrep = dir.path().join("ugrep");
    let bfs = dir.path().join("bfs");
    std::os::unix::fs::symlink(koda, &ugrep).unwrap();
    std::os::unix::fs::symlink(koda, &bfs).unwrap();
    (dir, ugrep, bfs)
}

fn run(program: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A git project with a tracked file, an ignored one and a hidden one.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(p)
        .status()
        .unwrap();
    std::fs::create_dir(p.join("src")).unwrap();
    std::fs::write(p.join("src/lib.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(p.join(".gitignore"), "ignored.rs\n").unwrap();
    std::fs::write(p.join("ignored.rs"), "fn needle() {}\n").unwrap();
    std::fs::write(p.join(".hidden.rs"), "fn needle() {}\n").unwrap();
    dir
}

/// Claude Code's `grep` function: ugrep as grep, honouring .gitignore,
/// searching hidden files, skipping binaries and VCS folders.
const CLAUDE_GREP: &[&str] = &[
    "-G",
    "--ignore-files",
    "--hidden",
    "-I",
    "--exclude-dir=.git",
    "--exclude-dir=.svn",
    "--exclude-dir=.hg",
    "--exclude-dir=.bzr",
    "--exclude-dir=.jj",
    "--exclude-dir=.sl",
];

#[test]
fn both_programs_report_the_build_claude_code_has() {
    let (dir, ugrep, bfs) = links();
    let v = run(&ugrep, dir.path(), &["-V"]);
    let text = stdout(&v);
    assert!(v.status.success(), "{v:?}");
    assert!(text.starts_with("ugrep 7.8.5 "), "{text}");
    assert!(
        text.contains("-P:pcre2jit; -z:zlib,bzip2,zstd,brotli,7z,tar/pax/cpio/zip"),
        "{text}"
    );
    let v = run(&bfs, dir.path(), &["-version"]);
    let text = stdout(&v);
    assert!(v.status.success(), "{v:?}");
    assert!(text.starts_with("bfs 4.1.7"), "{text}");
    assert!(
        text.contains(
            "--enable-release --with-oniguruma --without-libselinux --without-libacl \
             --without-liburing --without-libcap"
        ),
        "{text}"
    );
}

#[test]
fn grep_as_claude_code_runs_it() {
    let (_links, ugrep, _) = links();
    let p = project();
    let mut args = CLAUDE_GREP.to_vec();
    args.extend(["-rn", "needle", "."]);
    let o = run(&ugrep, p.path(), &args);
    let text = stdout(&o);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(text.contains("src/lib.rs:1:fn needle"), "{text}");
    assert!(text.contains(".hidden.rs:1:fn needle"), "{text}");
    assert!(!text.contains("ignored.rs"), "{text}");

    // grep's exit statuses: 1 for no match, 2 for an error.
    let mut none = CLAUDE_GREP.to_vec();
    none.extend(["-rn", "haystack", "."]);
    assert_eq!(run(&ugrep, p.path(), &none).status.code(), Some(1));
    assert_eq!(
        run(&ugrep, p.path(), &["-E", "(", "."]).status.code(),
        Some(2)
    );

    // Perl regexes, through PCRE2.
    let o = run(&ugrep, p.path(), &["-P", "-rn", r"need(?=le\(\))", "src"]);
    assert!(stdout(&o).contains("src/lib.rs:1:"), "{o:?}");
}

#[test]
fn grep_searches_inside_archives_with_z() {
    let (_links, ugrep, _) = links();
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(dir.path().join("a.zip")).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("inner.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"zipped needle\n").unwrap();
    zip.finish().unwrap();
    let o = run(&ugrep, dir.path(), &["-z", "-n", "needle", "a.zip"]);
    assert!(stdout(&o).contains("zipped needle"), "{o:?}");
}

#[test]
fn find_as_claude_code_runs_it() {
    let (_links, _, bfs) = links();
    let p = project();
    let o = run(
        &bfs,
        p.path(),
        &[
            "-S",
            "dfs",
            "-regextype",
            "findutils-default",
            ".",
            "-regex",
            r".*\.rs",
            "-not",
            "-path",
            "./.git/*",
        ],
    );
    let mut lines: Vec<String> = stdout(&o).lines().map(String::from).collect();
    lines.sort();
    assert!(o.status.success(), "{o:?}");
    assert_eq!(lines, ["./.hidden.rs", "./ignored.rs", "./src/lib.rs"]);

    let o = run(
        &bfs,
        p.path(),
        &["-S", "dfs", "src", "-type", "f", "-name", "*.rs"],
    );
    assert_eq!(stdout(&o), "src/lib.rs\n", "{o:?}");
}

#[test]
fn find_dies_of_a_closed_pipe_as_the_system_find_does() {
    // Rust starts with SIGPIPE ignored; the dispatch puts the default back.
    let (_links, _, bfs) = links();
    let dir = tempfile::tempdir().unwrap();
    for i in 0..4000 {
        std::fs::write(dir.path().join(format!("file-with-a-long-name-{i}")), "").unwrap();
    }
    let mut child = Command::new(&bfs)
        .arg(".")
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGPIPE), "{status:?}");
}

#[test]
fn only_those_names_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let other = dir.path().join("ugrep-old");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_koda"), &other).unwrap();
    let o = run(&other, dir.path(), &["--version"]);
    assert!(stdout(&o).contains("koda"), "{o:?}");
    assert!(!stdout(&o).contains("ugrep"), "{o:?}");
}
