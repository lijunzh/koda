//! # koda-git
//!
//! One way to run `git` for everything Koda runs itself, as Codex's
//! `codex-git-utils` gives its crates one: the git context at session start,
//! sub-agent worktrees and `/diff`. The model's own `git` commands go through
//! the Bash tool and the sandbox, not here.
//!
//! These commands run outside the sandbox, often on a tree a model wrote, and
//! a repository's own configuration (`.git/config`, attributes) can name
//! programs that git runs: hooks, an fsmonitor, clean and smudge filters, an
//! external diff, a textconv. So every command:
//!
//! - selects the repository from its working directory only: inherited
//!   selectors such as `GIT_DIR`, `GIT_WORK_TREE` and `GIT_CONFIG_PARAMETERS`
//!   are removed;
//! - turns hooks and the fsmonitor off, refuses implicit bare repositories
//!   (`safe.bareRepository=explicit`), ignores attribute files that config
//!   names, skips Git LFS downloads and never prompts;
//! - for a command that reads or writes the working tree
//!   ([`Operation::WorkingTree`]), turns off every filter driver the
//!   configuration defines.
//!
//! A diff also needs [`DIFF_ARGS`] (`--no-ext-diff --no-textconv`), which git
//! takes as arguments of `diff`, not as configuration.
//!
//! The settings are Codex's (`codex-rs/worktree/src/git.rs`).

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;

/// Arguments every `git diff` (and `git show`/`git log -p`) Koda runs takes:
/// no external diff program and no textconv filter.
pub const DIFF_ARGS: [&str; 2] = ["--no-ext-diff", "--no-textconv"];

/// Environment variables that select a repository or add configuration. Git
/// wrappers and hooks export them to their children; Koda's own git commands
/// select the repository from their working directory instead.
const INHERITED_SELECTORS: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// The configuration every command gets, as `-c` arguments.
const SAFE_CONFIG: [&str; 5] = [
    "safe.bareRepository=explicit",
    "core.hooksPath=/dev/null",
    "core.fsmonitor=",
    "attr.tree=",
    "core.attributesFile=",
];

/// What a command touches, which decides how much is turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Refs, objects and config only (`rev-parse`, `log`, `rev-list`,
    /// `branch -D`): no file in the working tree is read through a filter.
    Metadata,
    /// Reads or writes working-tree files (`status`, `diff`, `add`,
    /// `commit`, `worktree add`, `worktree remove`): the configuration's
    /// filter drivers are turned off as well, which costs one more `git
    /// config` call.
    WorkingTree,
}

/// A `git` command in `dir` for `operation`, hardened as the crate docs say.
/// Add the subcommand and its arguments.
///
/// For [`Operation::WorkingTree`] this first asks git which filter drivers
/// the configuration defines; that call's failure is the error.
pub fn command(dir: &Path, operation: Operation) -> io::Result<Command> {
    let mut command = base(dir);
    if operation == Operation::WorkingTree {
        command.envs(config_env(filter_overrides(configured_filters(dir)?)));
    }
    Ok(command)
}

/// [`command`] for tokio: the same command, run without blocking the
/// runtime, and killed if its future is dropped.
pub async fn tokio_command(
    dir: &Path,
    operation: Operation,
) -> io::Result<tokio::process::Command> {
    let dir = dir.to_path_buf();
    let command = tokio::task::spawn_blocking(move || command(&dir, operation))
        .await
        .map_err(io::Error::other)??;
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    Ok(command)
}

/// The repository's default branch, as Codex finds it without the network:
/// a remote's `HEAD` (`origin` first), else a local `main` or `master`.
/// `None` outside a repository or when none of them exists.
pub fn default_branch(dir: &Path) -> Option<String> {
    let stdout = |args: &[&str]| {
        base(dir)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let mut remotes: Vec<String> = stdout(&["remote"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    remotes.sort_by_key(|remote| remote != "origin");
    for remote in remotes {
        let head = format!("refs/remotes/{remote}/HEAD");
        if let Some(target) = stdout(&["symbolic-ref", "--quiet", &head])
            && let Some(name) = target.strip_prefix(&format!("refs/remotes/{remote}/"))
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|name| {
            stdout(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])
            .is_some()
        })
        .map(str::to_string)
}

/// Files in `dir`'s working tree that git doesn't track and doesn't ignore,
/// relative to `dir`. Empty outside a repository.
pub fn untracked_files(dir: &Path) -> Vec<String> {
    command(dir, Operation::WorkingTree)
        .ok()
        .and_then(|mut command| {
            command
                .args(["ls-files", "--others", "--exclude-standard", "-z"])
                .output()
                .ok()
        })
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter(|file| !file.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// An untracked `file` as a diff that adds it, as Codex's `/diff` shows
/// one: `git diff --no-index /dev/null <file>`.
pub fn untracked_diff(dir: &Path, file: &str) -> Option<String> {
    let out = command(dir, Operation::WorkingTree)
        .ok()?
        .args(["diff"])
        .args(DIFF_ARGS)
        .args(["--no-index", "--", "/dev/null", file])
        .output()
        .ok()?;
    // 1: the files differ, which they always do here.
    matches!(out.status.code(), Some(0 | 1))
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// The hardened command without the filter overrides.
fn base(dir: &Path) -> Command {
    let mut command = Command::new("git");
    for name in INHERITED_SELECTORS {
        command.env_remove(name);
    }
    for setting in SAFE_CONFIG {
        command.args(["-c", setting]);
    }
    command
        .current_dir(dir)
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// The filter drivers the configuration in effect in `dir` defines: names
/// from `filter.<driver>.clean`, `.smudge`, `.process` and `.required`.
/// Reading config runs no filter, hook or fsmonitor.
fn configured_filters(dir: &Path) -> io::Result<BTreeSet<String>> {
    let output = base(dir)
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\.",
        ])
        .output()?;
    if !output.status.success() {
        // 1: no key matched. Outside a repository git still reads the
        // global and system config, and exits 0 or 1 the same way.
        if output.status.code() == Some(1) {
            return Ok(BTreeSet::new());
        }
        return Err(io::Error::other(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(filter_names(&output.stdout))
}

/// The driver names in `git config --null --name-only` output.
fn filter_names(stdout: &[u8]) -> BTreeSet<String> {
    String::from_utf8_lossy(stdout)
        .split('\0')
        .filter_map(|key| {
            let (driver, attribute) = key.strip_prefix("filter.")?.rsplit_once('.')?;
            (!driver.is_empty() && matches!(attribute, "clean" | "smudge" | "process" | "required"))
                .then(|| driver.to_string())
        })
        .collect()
}

/// Settings that turn each of `filters` off.
fn filter_overrides(filters: BTreeSet<String>) -> Vec<(String, String)> {
    filters
        .into_iter()
        .flat_map(|filter| {
            [
                (format!("filter.{filter}.process"), String::new()),
                (format!("filter.{filter}.clean"), String::new()),
                (format!("filter.{filter}.smudge"), String::new()),
                (format!("filter.{filter}.required"), "false".to_string()),
            ]
        })
        .collect()
}

/// `overrides` as git's environment config (`GIT_CONFIG_COUNT`,
/// `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`), which takes a driver name
/// whatever characters it holds, unlike `-c`.
fn config_env(overrides: Vec<(String, String)>) -> Vec<(String, String)> {
    if overrides.is_empty() {
        return Vec::new();
    }
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), overrides.len().to_string())];
    for (index, (key, value)) in overrides.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let done = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(done.status.success(), "git {args:?}: {done:?}");
    }

    /// A repository whose own config names programs git would run: a hook,
    /// an fsmonitor, a clean and smudge filter, an external diff and a
    /// textconv. Each writes a marker file when it runs.
    fn hostile_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "first"]);

        let marker = tmp.path().join("ran");
        let program = tmp.path().join("evil.sh");
        fs::write(
            &program,
            format!("#!/bin/sh\necho \"$0 $*\" >> {}\ncat\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(
            &program,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let program = program.display().to_string();
        for (key, value) in [
            ("core.fsmonitor", program.as_str()),
            ("filter.evil.clean", program.as_str()),
            ("filter.evil.smudge", program.as_str()),
            ("filter.evil.required", "true"),
            ("diff.external", program.as_str()),
            ("diff.evil.textconv", program.as_str()),
        ] {
            git(&repo, &["config", key, value]);
        }
        fs::write(repo.join(".gitattributes"), "*.txt filter=evil diff=evil\n").unwrap();
        let hooks = repo.join(".git/hooks");
        fs::write(hooks.join("pre-commit"), format!("#!/bin/sh\n{program}\n")).unwrap();
        fs::set_permissions(
            hooks.join("pre-commit"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        // Touch the file so status and diff have to look at its content.
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        (tmp, repo)
    }

    fn ran(repo: &Path) -> String {
        fs::read_to_string(repo.parent().unwrap().join("ran")).unwrap_or_default()
    }

    #[test]
    fn plain_git_runs_the_repositorys_programs() {
        // The control: without Koda's settings, the same commands run them.
        let (_tmp, repo) = hostile_repo();
        Command::new("git")
            .args(["diff"])
            .current_dir(&repo)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(!ran(&repo).is_empty(), "the control ran nothing");
    }

    #[test]
    fn status_diff_add_and_commit_run_none_of_them() {
        let (_tmp, repo) = hostile_repo();
        let run = |args: &[&str]| {
            let out = command(&repo, Operation::WorkingTree)
                .unwrap()
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        assert!(run(&["status", "--short"]).contains("a.txt"));
        let mut diff = vec!["diff"];
        diff.extend(DIFF_ARGS);
        assert!(run(&diff).contains("+two"));
        let mut stat = vec!["diff", "--stat"];
        stat.extend(DIFF_ARGS);
        assert!(run(&stat).contains("a.txt"));
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "second"]);
        assert_eq!(ran(&repo), "", "a repository program ran");
    }

    #[test]
    fn a_worktree_checkout_runs_no_smudge_filter() {
        let (tmp, repo) = hostile_repo();
        let wt = tmp.path().join("wt");
        let out = command(&repo, Operation::WorkingTree)
            .unwrap()
            .args(["worktree", "add", "-q", "-b", "side"])
            .arg(&wt)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
        assert_eq!(ran(&repo), "", "a repository program ran");
    }

    #[test]
    fn inherited_selectors_are_removed() {
        // Setting them in this process would race the other tests; the
        // command's own environment says they're removed.
        let command = command(Path::new("."), Operation::Metadata).unwrap();
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().to_string())
            .collect();
        for name in INHERITED_SELECTORS {
            assert!(removed.iter().any(|key| key == name), "{name} kept");
        }
    }

    #[test]
    fn default_branch_prefers_the_remotes_head_then_main() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q", "-b", "trunk"]);
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let clone = tmp.path().join("clone");
        git(
            tmp.path(),
            &["clone", "-q", origin.to_str().unwrap(), "clone"],
        );
        git(&clone, &["checkout", "-q", "-b", "feature"]);
        assert_eq!(default_branch(&clone).as_deref(), Some("trunk"));
        // No remote: a local main.
        assert_eq!(default_branch(&origin), None);
        git(&origin, &["branch", "main"]);
        assert_eq!(default_branch(&origin).as_deref(), Some("main"));
        // Outside a repository.
        assert_eq!(default_branch(tmp.path()), None);
    }

    #[test]
    fn untracked_files_show_as_added_and_run_nothing() {
        let (_tmp, repo) = hostile_repo();
        fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(repo.join("ignored.txt"), "x\n").unwrap();
        let files = untracked_files(&repo);
        assert!(files.contains(&"new.txt".to_string()), "{files:?}");
        assert!(!files.contains(&"ignored.txt".to_string()), "{files:?}");
        let diff = untracked_diff(&repo, "new.txt").unwrap();
        assert!(diff.contains("+fresh"), "{diff}");
        assert_eq!(ran(&repo), "", "a repository program ran");
    }

    #[test]
    fn filter_names_reads_driver_names_with_dots() {
        let names = filter_names(
            b"filter.lfs.clean\0filter.lfs.smudge\0filter.a.b.process\0filter..clean\0filter.x.other\0",
        );
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["a.b".to_string(), "lfs".to_string()]
        );
    }

    #[test]
    fn config_env_numbers_each_setting() {
        assert!(config_env(Vec::new()).is_empty());
        let env = config_env(filter_overrides(BTreeSet::from(["lfs".to_string()])));
        assert_eq!(env[0], ("GIT_CONFIG_COUNT".into(), "4".into()));
        assert!(env.contains(&("GIT_CONFIG_KEY_3".into(), "filter.lfs.required".into())));
        assert!(env.contains(&("GIT_CONFIG_VALUE_3".into(), "false".into())));
    }

    #[tokio::test]
    async fn tokio_command_is_the_same_command() {
        let (_tmp, repo) = hostile_repo();
        let out = tokio_command(&repo, Operation::WorkingTree)
            .await
            .unwrap()
            .args(["status", "--short"])
            .output()
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("a.txt"));
        assert_eq!(ran(&repo), "");
    }
}
