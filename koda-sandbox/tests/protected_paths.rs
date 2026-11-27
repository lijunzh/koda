//! A sandboxed command can't get past the protection of `.git/hooks`,
//! `.git/config`, `.koda/agents` and `.koda/skills` by moving a folder above
//! them, as Codex's `nested_protected_paths_cannot_be_bypassed_by_renaming_ancestors`
//! checks for its own.
//!
//! The rules name those paths, so a command that renames `.git` and makes a
//! new one has an unprotected copy where git looks: a hook planted there
//! runs outside the sandbox at the next `git commit`. The commands run in the
//! real sandbox of this platform (seatbelt on macOS, bubblewrap on Linux).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use koda_sandbox::{Loopback, SandboxPolicy, SandboxTransformRequest};

fn available() -> bool {
    #[cfg(target_os = "linux")]
    return koda_sandbox::bwrap::is_available();
    #[cfg(not(target_os = "linux"))]
    true
}

/// A git repository with a commit, and `.koda/agents` and `.koda/skills`.
fn project() -> (tempfile::TempDir, PathBuf) {
    // Not in the temp folder, which every sandboxed command may write.
    let dir = tempfile::Builder::new()
        .prefix("protected-paths-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}: {status:?}");
    };
    git(&["init", "-q"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "one"]);
    std::fs::create_dir_all(root.join(".koda/agents")).unwrap();
    std::fs::create_dir_all(root.join(".koda/skills")).unwrap();
    (dir, root)
}

/// Run `script` in the sandbox at `root`; what it printed.
async fn run(root: &Path, script: &str) -> String {
    let policy = SandboxPolicy::strict_default();
    let request = SandboxTransformRequest {
        command: script,
        project_root: root,
        policy: &policy,
        proxy_port: None,
        loopback: &Loopback::default(),
    };
    let mut command = koda_sandbox::current_runtime()
        .transform(request)
        .unwrap()
        .command;
    let output = command.output().await.unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_folder_cannot_be_moved_and_replaced() {
    if !available() {
        return;
    }
    let (_dir, root) = project();
    let planted = [
        // Move `.git` aside and make a new one with a hook.
        "mv .git old && mkdir -p .git/hooks && cp old/config .git/config \
         && echo evil > .git/hooks/pre-commit",
        // Move it aside, plant the hook in the moved copy, move it back.
        "mv .git moved; echo evil > moved/hooks/pre-commit; mv moved .git",
        // The same for Koda's agent definitions and skills.
        "mv .koda k && mkdir -p .koda/agents && echo evil > .koda/agents/evil.json",
        "mv .koda/skills s && mkdir -p .koda/skills && echo evil > .koda/skills/SKILL.md",
    ];
    for script in planted {
        let said = run(&root, script).await;
        // Nothing where git and Koda read it. A copy elsewhere is harmless:
        // on Linux `mv` copies a folder it can't rename and then can't
        // remove the original, so `s` may hold a copy of the skills.
        for path in [
            ".git/hooks/pre-commit",
            ".koda/agents/evil.json",
            ".koda/skills/SKILL.md",
        ] {
            assert!(
                !root.join(path).exists(),
                "{path} after `{script}`:\n{said}"
            );
        }
        // And the protected folders are still the ones in place.
        for folder in [".git/hooks", ".koda/agents", ".koda/skills"] {
            assert!(
                root.join(folder).is_dir(),
                "{folder} is gone after `{script}`:\n{said}"
            );
        }
        let log = Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            log.status.success() && String::from_utf8_lossy(&log.stdout).lines().count() == 1,
            "the repository broke after `{script}`: {log:?}\n{said}"
        );
    }
}

/// The protection leaves ordinary work alone: files in the project, and git.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_still_writes_the_project_and_commits() {
    if !available() {
        return;
    }
    let (_dir, root) = project();
    let said = run(
        &root,
        "echo b > b.txt && mkdir -p src/x && mv src/x src/y && git add b.txt \
         && git -c user.name=t -c user.email=t@t commit -qm two && git log --oneline | wc -l",
    )
    .await;
    assert!(root.join("src/y").is_dir(), "{said}");
    assert_eq!(said.trim(), "2", "{said}");
}
