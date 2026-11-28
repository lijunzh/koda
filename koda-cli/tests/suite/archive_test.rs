//! `koda archive <SESSION>` and `koda unarchive <SESSION>` as processes, on
//! a host in their own process (`--no-daemon`). On the shared server, with
//! a window that has the session open, they are in `tui_pty_test.rs`; what
//! the host does is in `koda-server/tests/socket.rs`.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use koda_state::session_store::{SessionOrigin, SessionStore, source};

fn koda_bin() -> String {
    env!("CARGO_BIN_EXE_koda").to_string()
}

/// `koda` with `args`, its config folder under `root`.
fn koda(root: &Path, args: &[&str]) -> Output {
    Command::new(koda_bin())
        .args(args)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root)
        // No real provider is ever called: a key from the developer's shell
        // would send the host's Models API lookup to Anthropic.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn said(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    )
}

/// `koda archive` puts a session away and `koda unarchive` brings it back,
/// each saying which session by its full id, as `codex archive` and
/// `codex unarchive` do. A shortened id is looked for among the project's
/// sessions that aren't archived, or that are, so each refuses the other's;
/// a full id is the host's to refuse.
#[tokio::test(flavor = "multi_thread")]
async fn archive_and_unarchive_a_session_by_id() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = std::fs::canonicalize(&project).unwrap();
    let db = koda_state::db::Database::init(&root.path().join("koda"))
        .await
        .unwrap();
    let session = db
        .create_session_from(
            "default",
            &project,
            &SessionOrigin {
                source: Some(source::CLI.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    db.set_session_title(&session, "Parser work").await.unwrap();
    let archived = async || {
        let stored = db.get_session(&session).await.unwrap().unwrap();
        stored.archived_unix.is_some()
    };
    let short = &session[..8];
    let folder = project.to_str().unwrap();
    let run = |command: &str, id: &str| {
        said(&koda(
            root.path(),
            &["-C", folder, command, "--no-daemon", id],
        ))
    };

    let (code, out, err) = run("archive", short);
    assert_eq!(
        (code, out.as_str()),
        (Some(0), format!("Archived session {session}.").as_str()),
        "{err}"
    );
    assert!(archived().await);

    // Archived: not among the sessions `koda archive` looks in, and the
    // host refuses its full id.
    let (code, _, err) = run("archive", short);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains(&format!("No active session found matching '{short}'.")),
        "{err}"
    );
    let (code, _, err) = run("archive", &session);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains(&format!("session {session} is archived")),
        "{err}"
    );

    let (code, out, err) = run("unarchive", short);
    assert_eq!(
        (code, out.as_str()),
        (
            Some(0),
            format!("Unarchived session Parser work ({session}).").as_str()
        ),
        "{err}"
    );
    assert!(!archived().await);

    let (code, _, err) = run("unarchive", short);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains(&format!("No archived session found matching '{short}'.")),
        "{err}"
    );
    let (code, _, err) = run("unarchive", &session);
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains(&format!("session {session} is not archived")),
        "{err}"
    );
}

/// The two commands open no session: a flag that picks one's model or
/// mode, or a prompt, is refused rather than ignored, and they need the
/// session.
#[test]
fn archive_takes_a_session_and_no_session_flags() {
    let root = tempfile::tempdir().unwrap();
    for (args, wanted) in [
        (
            &["--model", "stub", "archive", "abc"][..],
            "koda archive doesn't take --model",
        ),
        (
            &["--mode", "plan", "unarchive", "abc"],
            "koda unarchive doesn't take --mode",
        ),
        (
            &[
                "--no-daemon",
                "archive",
                "--remote",
                "unix:///k.sock",
                "abc",
            ],
            "give --no-daemon or --remote, not both",
        ),
        (&["archive"], "<SESSION>"),
    ] {
        let (code, _, err) = said(&koda(root.path(), args));
        assert_eq!(code, Some(2), "{args:?}: {err}");
        assert!(err.contains(wanted), "{args:?}: {err}");
    }
}
