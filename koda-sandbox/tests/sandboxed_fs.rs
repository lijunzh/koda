//! Integration tests for [`WorkerClient`] and [`SandboxedFileSystem`]
//! (Phase 2c of #934).
//!
//! These live in `tests/` (not `src/`) because `CARGO_BIN_EXE_*` is
//! only injected by Cargo into integration test binaries — unit tests
//! in `src/#[cfg(test)]` blocks don't have access to it.

#[cfg(unix)]
mod unix {
    use koda_sandbox::fs::{FileSystem, FsError, SandboxedFileSystem};
    use koda_sandbox::ipc::{Request, Response};
    use koda_sandbox::worker_client::WorkerClient;
    use std::path::Path;
    use tempfile::TempDir;

    // ── WorkerClient ─────────────────────────────────────────────────

    #[tokio::test]
    async fn client_spawns_and_responds_to_ping() {
        let mut client = WorkerClient::spawn().await.expect("spawn");
        let resp = client.request(&Request::Ping).await.expect("ping");
        assert_eq!(resp, Response::Pong);
    }

    #[tokio::test]
    async fn client_reads_file_over_socket() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"socket ipc works").unwrap();

        let mut client = WorkerClient::spawn().await.expect("spawn");
        let resp = client
            .request(&Request::Read {
                path,
                max_bytes: None,
            })
            .await
            .expect("read");
        assert_eq!(
            resp,
            Response::Read {
                content: b"socket ipc works".to_vec()
            }
        );
    }

    #[tokio::test]
    async fn client_cleans_up_socket_on_drop() {
        let client = WorkerClient::spawn().await.expect("spawn");
        let path = client.socket_path().to_path_buf();
        assert!(path.exists());
        drop(client);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!path.exists(), "socket file must be removed on drop");
    }

    // ── SandboxedFileSystem ──────────────────────────────────────────

    #[tokio::test]
    async fn sandboxed_read_write_roundtrip() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rw.txt");

        let n = fs.write(&path, b"sandbox test").await.expect("write");
        assert_eq!(n, 12);

        let got = fs.read(&path, None).await.expect("read");
        assert_eq!(got, b"sandbox test");
    }

    #[tokio::test]
    async fn sandboxed_edit() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("e.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let n = fs.edit(&path, "world", "rust", false).await.expect("edit");
        assert_eq!(n, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello rust");
    }

    #[tokio::test]
    async fn sandboxed_glob() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"").unwrap();

        let paths = fs.glob("*.rs", dir.path()).await.expect("glob");
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn sandboxed_grep() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"needle\nhaystack\nneedle\n").unwrap();

        let hits = fs.grep("needle", dir.path(), None).await.expect("grep");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[1].line, 3);
    }

    #[tokio::test]
    async fn sandboxed_stat_file() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.txt");
        std::fs::write(&path, b"abcdef").unwrap();

        let m = fs.stat(&path).await.expect("stat");
        assert_eq!(m.size, 6);
        assert!(!m.is_dir);
        assert!(!m.is_symlink);
    }

    #[tokio::test]
    async fn sandboxed_fs_clones_share_connection() {
        // Two clones must be able to interleave calls without deadlock
        // or data race. The Arc<Mutex<>> wrapper serialises them.
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let fs2 = fs.clone();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("shared.txt");
        std::fs::write(&path, b"shared").unwrap();

        // tokio::join! runs both futures concurrently; the Mutex
        // ensures they don't overlap on the socket.
        let (r1, r2) = tokio::join!(fs.read(&path, None), fs2.read(&path, None));
        assert_eq!(r1.unwrap(), b"shared");
        assert_eq!(r2.unwrap(), b"shared");
    }

    #[tokio::test]
    async fn sandboxed_read_missing_file_returns_io_err() {
        let fs = SandboxedFileSystem::spawn().await.expect("spawn");
        let dir = TempDir::new().unwrap();
        let err = fs
            .read(Path::new(&dir.path().join("ghost")), None)
            .await
            .expect_err("must fail");
        assert!(matches!(err, FsError::Io(_)));
    }

    // ── Phase 2f: spawn_with_policy integration ──────────────────────

    #[tokio::test]
    async fn spawn_with_policy_write_inside_root_succeeds() {
        use koda_sandbox::ipc::{Request, Response};
        use koda_sandbox::policy::SandboxPolicy;

        let root = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let path = canonical_root.join("allowed.txt");

        let mut client =
            WorkerClient::spawn_with_policy(canonical_root.clone(), &SandboxPolicy::default())
                .await
                .expect("spawn_with_policy");

        let resp = client
            .request(&Request::Write {
                path: path.clone(),
                content: b"policy ok".to_vec(),
            })
            .await
            .expect("request");

        assert!(
            matches!(resp, Response::Write { .. }),
            "expected Write ok, got {resp:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"policy ok");
    }

    #[tokio::test]
    async fn spawn_with_policy_write_outside_root_is_denied() {
        use koda_sandbox::ipc::{ErrorCode, Request, Response};
        use koda_sandbox::policy::SandboxPolicy;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let path = std::fs::canonicalize(outside.path())
            .unwrap()
            .join("escape.txt");

        let mut client = WorkerClient::spawn_with_policy(canonical_root, &SandboxPolicy::default())
            .await
            .expect("spawn_with_policy");

        let resp = client
            .request(&Request::Write {
                path,
                content: b"evil".to_vec(),
            })
            .await
            .expect("request");

        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::PolicyDenied,
                    ..
                }
            ),
            "expected PolicyDenied, got {resp:?}"
        );
    }

    #[tokio::test]
    async fn spawn_with_policy_symlink_escape_is_denied() {
        use koda_sandbox::ipc::{ErrorCode, Request, Response};
        use koda_sandbox::policy::SandboxPolicy;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let canonical_outside = std::fs::canonicalize(outside.path()).unwrap();

        // Symlink inside root pointing outside.
        let link = canonical_root.join("escape_link");
        std::os::unix::fs::symlink(&canonical_outside, &link).unwrap();

        let mut client = WorkerClient::spawn_with_policy(canonical_root, &SandboxPolicy::default())
            .await
            .expect("spawn_with_policy");

        let resp = client
            .request(&Request::Write {
                path: link.join("secret.txt"),
                content: b"evil".to_vec(),
            })
            .await
            .expect("request");

        assert!(
            matches!(
                resp,
                Response::Error {
                    code: ErrorCode::PolicyDenied,
                    ..
                }
            ),
            "symlink escape should be denied, got {resp:?}"
        );
    }
}
