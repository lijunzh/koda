//! Search through the shell, as Claude Code does (`shell_search`, on by
//! default).
//!
//! Claude Code drops its `Grep` and `Glob` tools when it can run its
//! embedded search programs, and its prompt tells the model to use `grep`
//! and `find` through Bash. Its shell gets two functions: `grep` runs its
//! embedded ugrep (`-G --ignore-files --hidden -I`, skipping
//! version-control folders), and `find` runs its embedded bfs (`-S dfs
//! -regextype findutils-default`). Options ugrep handles differently
//! (filters, pagers, `-Z`, `--null`) go to the system `grep`, and so does
//! everything when its executable can't be found.
//!
//! Koda does the same: `koda` carries ugrep and bfs (`koda-search`), `Grep`,
//! `Glob` and `List` leave the tool list (Claude Code has no `List`; Codex
//! has none of the three), the prompt's search guidance and Bash's
//! description become Claude Code's, and every Bash command starts with
//! [`prelude`]. Where Claude Code's functions start its executable under
//! another name with `exec -a`, Koda's pass a first argument
//! ([`koda_search::Program::flag`]), as Codex starts its own for
//! `apply_patch`: Koda runs commands with `sh`, which on Debian and Ubuntu
//! is dash, and dash has no `exec -a`.
//!
//! It's on only where the programs are: in the `koda` binary, which records
//! itself at start ([`koda_search::exe`]). `shell_search = false` in the
//! config file brings the tools back, as Claude Code's launch option does.

use std::path::Path;

/// The tools search through the shell replaces.
pub const REPLACED_TOOLS: [&str; 3] = ["Grep", "Glob", "List"];

/// Whether `name` is one of [`REPLACED_TOOLS`].
pub fn replaces(name: &str) -> bool {
    REPLACED_TOOLS.contains(&name)
}

/// Whether this process can run ugrep and bfs, so search can go through
/// the shell: it is the `koda` binary.
pub fn available() -> bool {
    koda_search::exe().is_some()
}

/// `s` quoted for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Shell functions defined before every Bash command: Claude Code's `grep`
/// and `find`, with its arguments, running `exe`'s ugrep and bfs, or the
/// system's `grep` and `find` when `exe` is gone (an upgrade replaced it).
pub fn prelude(exe: &Path) -> String {
    use koda_search::Program;
    let exe = sh_quote(&exe.to_string_lossy());
    let ugrep = Program::Ugrep.flag();
    let bfs = Program::Bfs.flag();
    format!(
        r#"unalias find 2>/dev/null || true
unalias grep 2>/dev/null || true
grep() {{
  for _koda_a in "$@"; do
    case "$_koda_a" in -*-filter*|-*-pager*|-*-view*|-*-format-open*|-*-config*|---*|-@*|-*-save-config*|-[Zz]*|-[!-]*[Zz]*|--null|--null-data) command grep "$@"; return ;; esac
  done
  if [ ! -x {exe} ]; then command grep "$@"; return; fi
  {exe} {ugrep} -G --ignore-files --hidden -I --exclude-dir=.git --exclude-dir=.svn --exclude-dir=.hg --exclude-dir=.bzr --exclude-dir=.jj --exclude-dir=.sl "$@"
}}
find() {{
  if [ ! -x {exe} ]; then command find "$@"; return; fi
  {exe} {bfs} -S dfs -regextype findutils-default "$@"
}}
"#
    )
}

/// `command` with [`prelude`] in front, when this process carries the
/// programs; otherwise `command` as it is.
pub fn wrap(command: &str) -> String {
    match koda_search::exe() {
        Some(exe) => format!("{}{command}", prelude(exe)),
        None => command.to_string(),
    }
}

/// Bash's description, with its guidance on the replaced tools rewritten
/// as Claude Code's reads when it searches through the shell.
const BASH_EDITS: &[(&str, &str)] = &[
    (
        "Use ONLY for builds, tests, git, \
         and commands without a dedicated tool. Never use for file ops \
         (use Read/Write/Edit/Grep/List instead).",
        "IMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, \
         `awk`, or `echo` commands, unless explicitly instructed or after you \
         have verified that a dedicated tool cannot accomplish your task. \
         Instead, use the appropriate dedicated tool (`Read`, `Edit`, `Write`) \
         as this will provide a much better experience for the user.",
    ),
    (
        "narrow the command (grep, sed -n 'A,Bp')",
        "narrow the command (grep)",
    ),
];

/// The tool list for search through the shell: without [`REPLACED_TOOLS`],
/// and with Bash's description as [`BASH_EDITS`] makes it.
pub fn adapt_tool_defs(defs: &mut Vec<crate::providers::ToolDefinition>) {
    defs.retain(|d| !replaces(&d.name));
    for d in defs.iter_mut().filter(|d| d.name == "Bash") {
        for (from, to) in BASH_EDITS {
            d.description = d.description.replace(from, to);
        }
    }
}

/// Pairs of (text naming the replaced tools, its shell-search version), in
/// the system prompt and the built-in agents' prompts.
const PROMPT_EDITS: &[(&str, &str)] = &[
    // instructions.md
    (
        "`Read` not `cat` | `Grep` not `rg` | `List` not `ls`/`find` | `Edit` not `sed`\n\
         Reserve `Bash` for builds, tests, git, and commands without a dedicated tool.",
        "`Read` not `cat` | `Edit` not `sed`\n\
         Use `grep` via `Bash` for searching file contents with regex, and `find` via `Bash` \
         for broad file pattern matching.\n\
         Reserve `Bash` for search, builds, tests, git, and commands without a dedicated tool.",
    ),
    // agents/explore.json
    (
        "- `Grep`  \u{2014} search file *contents* with regex (e.g. find all callers of a function, locate a type definition)\n\
         - `Glob`  \u{2014} find files by *name pattern* (e.g. `**/*.rs`, `src/tools/mod.rs`)\n\
         - `Read`  \u{2014} read a *specific file* you already know the path to\n\
         - `List`  \u{2014} browse a *directory* to understand structure\n\
         - `Bash`  \u{2014} ONLY for read-only commands: `git log`, `git diff`, `git show`, `wc`, `head`, `tail`, `cat`;",
        "- `grep` via `Bash` \u{2014} search file *contents* with regex (e.g. find all callers of a function, locate a type definition)\n\
         - `find` via `Bash` \u{2014} find files by *name pattern* (e.g. `find src -name '*.rs'`)\n\
         - `Read`  \u{2014} read a *specific file* you already know the path to\n\
         - `Bash`  \u{2014} ONLY for read-only commands: `grep`, `find`, `ls`, `git log`, `git diff`, `git show`, `wc`, `head`, `tail`, `cat`;",
    ),
    (
        "1. Start broad \u{2014} `List` the root and key directories to understand layout\n\
         2. Use `Glob` to locate files by name pattern before reading them\n\
         3. Use `Grep` for content searches; prefer specific patterns over broad ones",
        "1. Start broad \u{2014} `ls` the root and key directories to understand layout\n\
         2. Use `find` to locate files by name pattern before reading them\n\
         3. Use `grep` for content searches; prefer specific patterns over broad ones",
    ),
    // agents/task.json
    (
        "- Use `Grep` to search file contents (function names, types, patterns)\n\
         - Use `Glob` to locate files by name pattern\n\
         - Use `Read` once you know the specific file\n\
         - Use `List` to understand directory structure\n",
        "- Use `grep` via `Bash` to search file contents (function names, types, patterns)\n\
         - Use `find` via `Bash` to locate files by name pattern\n\
         - Use `Read` once you know the specific file\n",
    ),
];

/// `prompt` with its guidance on the replaced tools rewritten for search
/// through the shell.
pub fn adapt_prompt(prompt: &str) -> String {
    let mut out = prompt.to_string();
    for (from, to) in PROMPT_EDITS {
        out = out.replace(from, to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_edit_matches_the_text_it_replaces() {
        let instructions = include_str!("instructions.md");
        let explore = agent_prompt(include_str!("../agents/explore.json"));
        let task = agent_prompt(include_str!("../agents/task.json"));
        for (from, _) in PROMPT_EDITS {
            assert!(
                [&instructions.to_string(), &explore, &task]
                    .iter()
                    .any(|p| p.contains(from)),
                "no prompt contains {from:?}"
            );
        }
        for prompt in [instructions.to_string(), explore, task] {
            let adapted = adapt_prompt(&prompt);
            for tool in REPLACED_TOOLS {
                assert!(
                    !adapted.contains(&format!("`{tool}`")),
                    "{tool} still named in {adapted}"
                );
            }
        }
    }

    #[test]
    fn bash_reads_as_claude_codes_and_the_replaced_tools_go() {
        let mut defs = crate::tools::shell::definitions();
        let bash = defs[0].description.clone();
        for (from, _) in BASH_EDITS {
            assert!(bash.contains(from), "Bash's description lacks {from:?}");
        }
        defs.extend(
            REPLACED_TOOLS
                .iter()
                .map(|name| crate::providers::ToolDefinition {
                    name: name.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }),
        );
        adapt_tool_defs(&mut defs);
        assert_eq!(defs.len(), 1, "{defs:?}");
        let bash = &defs[0].description;
        assert!(
            bash.contains("Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands"),
            "{bash}"
        );
        for tool in ["Grep", "List", "Glob", "sed -n"] {
            assert!(!bash.contains(tool), "{tool} still in {bash}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_replaced_tools_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tools::ToolRegistry::new(dir.path().canonicalize().unwrap(), 100_000);
        registry.set_shell_search(true);
        for name in REPLACED_TOOLS {
            let call = registry.execute(name, "{}", None, None).await;
            assert!(!call.success, "{name}: {call:?}");
            assert!(call.output.contains("Unknown tool"), "{name}: {call:?}");
        }
        registry.set_shell_search(false);
        let list = registry.execute("List", "{}", None, None).await;
        assert!(!list.output.contains("Unknown tool"), "{list:?}");
    }

    /// The shells the prelude has to work in: `sh` (bash on macOS, dash on
    /// Debian and Ubuntu), and dash and bash wherever they are installed.
    fn shells() -> Vec<&'static str> {
        ["sh", "dash", "bash", "zsh"]
            .into_iter()
            .filter(|s| {
                std::process::Command::new(s)
                    .args(["-c", "true"])
                    .status()
                    .is_ok_and(|st| st.success())
            })
            .collect()
    }

    fn run_in(shell: &str, script: &str, cwd: &Path) -> (String, Option<i32>) {
        let out = std::process::Command::new(shell)
            .args(["-c", script])
            .current_dir(cwd)
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code(),
        )
    }

    #[test]
    fn the_functions_pass_claude_codes_arguments_in_every_shell() {
        // A stand-in for `koda` that prints the arguments it gets.
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake koda's exe");
        std::fs::write(&exe, "#!/bin/sh\nprintf '[%s]' \"$@\"\necho\nexit 3\n").unwrap();
        let mut perm = std::fs::metadata(&exe).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&exe, perm).unwrap();
        let prelude = prelude(&exe);
        assert!(!prelude.contains("exec -a"), "{prelude}");
        let shells = shells();
        assert!(shells.contains(&"sh"));
        for shell in shells {
            let (out, code) = run_in(shell, &format!("{prelude}grep -rn 'a b' ."), dir.path());
            assert_eq!(
                out.trim(),
                "[--koda-run-as-ugrep][-G][--ignore-files][--hidden][-I][--exclude-dir=.git]\
                 [--exclude-dir=.svn][--exclude-dir=.hg][--exclude-dir=.bzr][--exclude-dir=.jj]\
                 [--exclude-dir=.sl][-rn][a b][.]",
                "{shell}"
            );
            assert_eq!(code, Some(3), "{shell}: the program's status comes back");
            let (out, _) = run_in(shell, &format!("{prelude}find . -name '*.rs'"), dir.path());
            assert_eq!(
                out.trim(),
                "[--koda-run-as-bfs][-S][dfs][-regextype][findutils-default][.][-name][*.rs]",
                "{shell}"
            );
            // Options ugrep reads differently go to the system's grep.
            let (out, _) = run_in(
                shell,
                &format!("{prelude}printf 'x\\n' | grep -Z x"),
                dir.path(),
            );
            assert!(!out.contains("--koda-run-as-ugrep"), "{shell}: {out}");
        }
    }

    #[test]
    fn without_the_executable_the_system_programs_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn needle() {}\n").unwrap();
        let prelude = prelude(&dir.path().join("gone"));
        for shell in shells() {
            let (out, code) = run_in(shell, &format!("{prelude}grep -rn needle ."), dir.path());
            assert!(out.contains("a.rs:1:fn needle"), "{shell}: {out}");
            assert_eq!(code, Some(0), "{shell}");
            let (out, _) = run_in(shell, &format!("{prelude}find . -name '*.rs'"), dir.path());
            assert_eq!(out.trim(), "./a.rs", "{shell}");
        }
    }

    #[test]
    fn a_test_binary_has_no_programs_so_commands_run_as_written() {
        assert!(!available());
        assert_eq!(wrap("grep -rn x ."), "grep -rn x .");
    }

    fn agent_prompt(json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["system_prompt"].as_str().unwrap().to_string()
    }
}
