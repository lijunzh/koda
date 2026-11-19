//! Many sessions and several processes writing one session database at once,
//! as `koda serve`, parallel sub-agents and a `koda exec` beside it do, with
//! the write lock held past SQLite's five-second wait at the start, as a
//! stalled disk held it on CI (run 2510: a sub-agent's first write failed
//! with "database is locked"). Every write must land.
//!
//! The other processes are this test binary again, running the ignored
//! `writer_process` test against the same database.

use std::path::Path;
use std::time::Duration;

use koda_state::busy_retry::PoolWriteExt as _;
use koda_state::db::Database;
use koda_state::session_store::{Role, SessionStore, TurnRecord};

/// Sessions each process writes, and rounds of writes in each.
const SESSIONS: usize = 6;
const ROUNDS: usize = 15;
/// The other processes.
const PROCESSES: usize = 2;
/// How long the lock is held at the start: past `busy_timeout` (5 s), well
/// within one retry.
const STALL: Duration = Duration::from_millis(5_150);

const DB_ENV: &str = "KODA_CONCURRENT_WRITERS_DB";
const START_ENV: &str = "KODA_CONCURRENT_WRITERS_START";

/// One session's writes: what a turn stores, and a fork, in rounds.
async fn session_writes(db: Database, root: &Path, tag: String) -> anyhow::Result<String> {
    let session = db.create_session(&tag, root).await?;
    db.set_metadata(&session, "writer", &tag).await?;
    for round in 0..ROUNDS {
        db.insert_message(
            &session,
            &Role::User,
            Some(&format!("{tag} q{round}")),
            None,
            None,
            None,
        )
        .await?;
        db.insert_message(&session, &Role::Assistant, Some("a"), None, None, None)
            .await?;
        db.append_turn_records(
            &session,
            &[TurnRecord {
                turn_id: format!("{tag}-t{round}"),
                kind: "turn".into(),
                payload: "{}".into(),
            }],
        )
        .await?;
        db.insert_session_event(&session, "info", "note", None)
            .await?;
        db.kv_set(&format!("{tag}-k{round}"), "v").await?;
    }
    db.fork_session(&session, "fork", root).await?;
    Ok(session)
}

/// Every session of one process, at once.
async fn write_all(dir: &Path, process: &str) -> anyhow::Result<()> {
    let db = Database::init(dir).await?;
    let mut tasks = Vec::new();
    for s in 0..SESSIONS {
        let (db, root, tag) = (db.clone(), dir.to_path_buf(), format!("{process}-s{s}"));
        tasks.push(tokio::spawn(
            async move { session_writes(db, &root, tag).await },
        ));
    }
    for task in tasks {
        let session = task.await??;
        let messages = db.load_all_messages(&session).await?;
        anyhow::ensure!(
            messages.len() == 2 * ROUNDS,
            "{session}: {}",
            messages.len()
        );
    }
    Ok(())
}

/// Wait until the parent says go, so every process writes at once.
async fn wait_for_start(flag: &Path) {
    while !flag.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_sessions_and_processes_write_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::init(dir.path()).await.unwrap();
    let start = dir.path().join("start");

    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..PROCESSES)
        .map(|_| {
            std::process::Command::new(&exe)
                .args(["writer_process", "--exact", "--ignored", "--nocapture"])
                .env(DB_ENV, dir.path())
                .env(START_ENV, &start)
                .spawn()
                .unwrap()
        })
        .collect();

    // Hold the write lock past every writer's five-second wait, then let go.
    let mut stall = db.pool().begin_immediate().await.unwrap();
    std::fs::write(&start, "").unwrap();
    let ours = tokio::spawn({
        let dir = dir.path().to_path_buf();
        async move { write_all(&dir, "parent").await }
    });
    tokio::time::sleep(STALL).await;
    sqlx::query("INSERT INTO kv_store (key, value) VALUES ('stall', 'held')")
        .execute(&mut *stall)
        .await
        .unwrap();
    stall.commit().await.unwrap();

    ours.await.unwrap().unwrap();
    for mut child in children {
        let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
            .await
            .unwrap();
        assert!(status.success(), "a writer process failed: {status}");
    }

    let (sessions,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
        .fetch_one(db.pool())
        .await
        .unwrap();
    // Each session and its fork, in every process.
    assert_eq!(sessions as usize, (PROCESSES + 1) * SESSIONS * 2);
    let (keys,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM kv_store WHERE key != 'stall'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(keys as usize, (PROCESSES + 1) * SESSIONS * ROUNDS);
}

/// One of the other processes. Run only by the test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run by many_sessions_and_processes_write_at_once"]
async fn writer_process() {
    let (Some(dir), Some(start)) = (std::env::var_os(DB_ENV), std::env::var_os(START_ENV)) else {
        return;
    };
    wait_for_start(Path::new(&start)).await;
    let process = format!("child{}", std::process::id());
    write_all(Path::new(&dir), &process).await.unwrap();
}
