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

// ── Search through the shell, end to end ─────────────────────────────

use koda_test_utils::network::{FakeLlmServer, ResponseTemplate};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

/// One headless turn in `project` whose model runs `command` through Bash
/// once. `config` is written to config.toml first. Returns the chat
/// request bodies.
async fn bash_turn(project: &Path, command: &str, config: &str) -> Vec<String> {
    let server = FakeLlmServer::spawn().await;
    let turns = AtomicUsize::new(0);
    let call = json!({"command": command}).to_string();
    server
        .mount_fn("POST", r".*/chat/completions$", move |_| {
            if turns.fetch_add(1, Ordering::SeqCst) == 0 {
                sse(&[
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "Bash", "arguments": call},
                    }]}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                ])
            } else {
                sse(&[
                    json!({"choices": [{"delta": {"content": "Done."}}]}),
                    json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                ])
            }
        })
        .await;
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("koda")).unwrap();
    std::fs::write(home.path().join("koda/config.toml"), config).unwrap();
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_koda"));
    cmd.args(["exec", "Search"])
        .args(["--provider", "lmstudio", "--model", "stub"])
        .args(["--base-url", &server.url()])
        .arg("--project-root")
        .arg(project)
        .env("XDG_CONFIG_HOME", home.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .expect("koda hung")
        .expect("failed to run koda");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server
        .received_requests()
        .await
        .into_iter()
        .filter(|r| r.url.path().ends_with("/chat/completions"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect()
}

/// `type grep` said it's a function (bash: "a function", dash: "a shell
/// function").
fn is_function(body: &str) -> bool {
    body.contains("grep is a function") || body.contains("grep is a shell function")
}

fn tool_names(body: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(body).unwrap();
    v["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_searches_with_the_built_in_grep_by_default() {
    let p = project();
    let bodies = bash_turn(
        p.path(),
        "type grep | head -1; grep -rn needle .; find . -name '*.rs' -not -path './.git/*' | sort",
        "",
    )
    .await;
    assert!(bodies.len() >= 2, "{} model call(s)", bodies.len());
    let tools = tool_names(&bodies[0]);
    for gone in ["Grep", "Glob", "List"] {
        assert!(
            !tools.iter().any(|t| t == gone),
            "{gone} offered: {tools:?}"
        );
    }
    assert!(tools.iter().any(|t| t == "Bash"), "{tools:?}");
    assert!(
        bodies[0].contains(
            "Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands"
        ),
        "Bash's description isn't Claude Code's"
    );
    assert!(
        bodies[0].contains("`grep` via `Bash`"),
        "prompt not adapted"
    );
    // The tool result: ugrep through the sandbox, .gitignore honoured, and
    // the command shown as the model wrote it.
    let result = &bodies[1];
    assert!(
        is_function(result),
        "grep isn't the shell function: {result}"
    );
    assert!(result.contains("src/lib.rs:1:fn needle"), "{result}");
    assert!(result.contains(".hidden.rs:1:fn needle"), "{result}");
    assert!(!result.contains("ignored.rs:1"), "{result}");
    assert!(
        result.contains("./ignored.rs"),
        "find lists every file: {result}"
    );
    assert!(!result.contains("unalias"), "the prelude leaked: {result}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_search_false_brings_the_tools_back() {
    let p = project();
    let bodies = bash_turn(p.path(), "type grep | head -1", "shell_search = false\n").await;
    let tools = tool_names(&bodies[0]);
    for back in ["Grep", "Glob", "List"] {
        assert!(tools.iter().any(|t| t == back), "{back} missing: {tools:?}");
    }
    assert!(!is_function(&bodies[1]), "{}", bodies[1]);
}
