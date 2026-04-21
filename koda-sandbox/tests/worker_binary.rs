//! End-to-end integration tests for the `koda-fs-worker` binary
//! (updated Phase 2c of #934).
//!
//! Spawns the real binary over stdio (no Unix socket — that transport
//! is exercised by `sandboxed_fs.rs`). Validates the IPC framing and
//! handler dispatch across a real OS process boundary.

use koda_sandbox::ipc::{Request, Response, read_message, write_message};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Cargo writes binaries to `$CARGO_MANIFEST_DIR/../target/<profile>/`.
/// In integration tests, `CARGO_BIN_EXE_<name>` is the canonical way to
/// find them.
fn worker_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_koda-fs-worker"))
}

#[tokio::test]
async fn worker_binary_responds_to_ping() {
    let mut child = Command::new(worker_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn koda-fs-worker");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    write_message(&mut stdin, &Request::Ping)
        .await
        .expect("write ping");
    let resp: Response = read_message(&mut stdout)
        .await
        .expect("read response")
        .expect("response not None");
    assert_eq!(resp, Response::Pong);

    // Tell the worker to exit and wait for clean shutdown.
    write_message(&mut stdin, &Request::Shutdown)
        .await
        .expect("write shutdown");
    let _ack: Response = read_message(&mut stdout)
        .await
        .expect("read shutdown ack")
        .expect("ack not None");
    stdin.shutdown().await.ok();
    drop(stdin);

    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("worker must exit within 5s")
        .expect("wait failed");
    assert!(
        status.success(),
        "worker exited non-zero after Shutdown: {status:?}"
    );
}

#[tokio::test]
async fn worker_binary_handles_read_over_stdio() {
    use tempfile::TempDir;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hello.txt");
    std::fs::write(&path, b"binary test").unwrap();

    let mut child = Command::new(worker_binary())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn koda-fs-worker");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    write_message(
        &mut stdin,
        &Request::Read {
            path,
            max_bytes: None,
        },
    )
    .await
    .expect("write read");
    let resp: Response = read_message(&mut stdout)
        .await
        .expect("read response")
        .expect("response not None");
    assert_eq!(
        resp,
        Response::Read {
            content: b"binary test".to_vec()
        }
    );

    drop(stdin);
    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("worker must exit within 5s")
        .expect("wait");
}
