//! End-to-end integration test for the `koda-fs-worker` binary
//! (Phase 2a of #934).
//!
//! Spawns the real binary as a subprocess, talks to it over stdin/stdout
//! using the [`koda_sandbox::ipc`] framing, and asserts the contract
//! holds across the OS process boundary. The unit tests in
//! `koda-sandbox::worker::tests` exercise the same code path through
//! an in-memory `tokio::io::duplex`; this test is what catches issues
//! the in-memory transport hides (binary discoverability, child stderr,
//! tokio runtime setup, env-var tracing init).

use koda_sandbox::ipc::{ErrorCode, Request, Response, read_message, write_message};
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
async fn worker_binary_returns_unimplemented_for_phase2c_variants() {
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
            path: PathBuf::from("/etc/passwd"),
            max_bytes: None,
        },
    )
    .await
    .expect("write read");
    let resp: Response = read_message(&mut stdout)
        .await
        .expect("read response")
        .expect("response not None");
    match resp {
        Response::Error {
            code: ErrorCode::Unimplemented,
            ..
        } => {}
        other => panic!("expected Unimplemented, got {other:?}"),
    }

    // Drop stdin to trigger clean EOF shutdown — exercises the
    // "peer hung up" exit path of the dispatch loop in the real binary.
    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("worker must exit within 5s of EOF")
        .expect("wait failed");
    assert!(
        status.success(),
        "worker should exit cleanly on EOF, got {status:?}"
    );
}
