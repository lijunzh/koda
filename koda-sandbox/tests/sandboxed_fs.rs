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

    // ── Phase 3a: spawn_with_policy_and_proxy integration ──────────────

    /// Build the netcat listen-in-background command we use as a stand-in
    /// for a real proxy. macOS BSD `nc` and Linux netcat-openbsd both
    /// accept `-l -k <port>` (-k = stay alive across multiple connections,
    /// otherwise BSD nc one-shots and the wait_for_bind poll races to
    /// connect before nc exits).
    fn netcat_listen_command(port: u16) -> Vec<String> {
        vec![
            "nc".to_string(),
            "-l".to_string(),
            "-k".to_string(),
            port.to_string(),
        ]
    }

    #[tokio::test]
    async fn spawn_with_policy_and_proxy_injects_env_vars() {
        use koda_sandbox::ipc::{Request, Response};
        use koda_sandbox::policy::SandboxPolicy;
        use koda_sandbox::proxy::{DEFAULT_NO_PROXY, ExternalProxy};

        let root = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();

        // Spawn a fake proxy via ExternalProxy + nc. nc may not be installed
        // everywhere; skip the test if spawn fails for that reason.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let proxy_spec = ExternalProxy {
            command: netcat_listen_command(port),
            env: Default::default(),
            port: Some(port),
            startup_timeout: std::time::Duration::from_secs(2),
        };
        let proxy = match proxy_spec.spawn().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: nc unavailable or failed to bind: {e:#}");
                return;
            }
        };

        let mut client = WorkerClient::spawn_with_policy_and_proxy(
            canonical_root,
            &SandboxPolicy::default(),
            Some(&proxy),
        )
        .await
        .expect("spawn_with_policy_and_proxy");

        let resp = client
            .request(&Request::GetEnv {
                names: vec![
                    "HTTPS_PROXY".into(),
                    "https_proxy".into(),
                    "HTTP_PROXY".into(),
                    "NO_PROXY".into(),
                    "SSL_CERT_FILE".into(), // None: no MITM configured
                ],
            })
            .await
            .expect("GetEnv request");

        let values = match resp {
            Response::GetEnv { values } => values,
            other => panic!("expected GetEnv response, got {other:?}"),
        };
        assert_eq!(
            values[0].as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str())
        );
        assert_eq!(
            values[1].as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str())
        );
        assert_eq!(
            values[2].as_deref(),
            Some(format!("http://127.0.0.1:{port}").as_str())
        );
        assert_eq!(values[3].as_deref(), Some(DEFAULT_NO_PROXY));
        // SSL_CERT_FILE must not be injected by *us* when policy.net.mitm
        // is None. We can only assert this if the host doesn't already
        // have it set — e.g. Ubuntu CI runners ship with SSL_CERT_FILE
        // pointing at /etc/ssl/certs/ca-certificates.crt, which the worker
        // inherits through normal subprocess env propagation.
        if std::env::var("SSL_CERT_FILE").is_err() {
            assert_eq!(
                values[4], None,
                "SSL_CERT_FILE must be unset when policy.net.mitm is None"
            );
        } else {
            // Host had it set — verify we didn't *override* it with our own
            // path (which would only happen with a CA bundle in policy).
            assert_eq!(
                values[4],
                std::env::var("SSL_CERT_FILE").ok(),
                "SSL_CERT_FILE must be the host's value, not overridden"
            );
        }
    }

    #[tokio::test]
    async fn spawn_with_policy_and_proxy_injects_ca_bundle_when_mitm_set() {
        use koda_sandbox::ipc::{Request, Response};
        use koda_sandbox::policy::{MitmConfig, NetPolicy, SandboxPolicy};
        use koda_sandbox::proxy::ExternalProxy;
        use std::path::PathBuf;

        let root = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();

        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let proxy_spec = ExternalProxy {
            command: netcat_listen_command(port),
            env: Default::default(),
            port: Some(port),
            startup_timeout: std::time::Duration::from_secs(2),
        };
        let proxy = match proxy_spec.spawn().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping: nc unavailable or failed to bind: {e:#}");
                return;
            }
        };

        let policy = SandboxPolicy {
            net: NetPolicy {
                mitm: Some(MitmConfig {
                    ca_bundle: PathBuf::from("/etc/ssl/corp-ca.pem"),
                    socket_map: vec![],
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let mut client =
            WorkerClient::spawn_with_policy_and_proxy(canonical_root, &policy, Some(&proxy))
                .await
                .expect("spawn_with_policy_and_proxy");

        let resp = client
            .request(&Request::GetEnv {
                names: vec![
                    "SSL_CERT_FILE".into(),
                    "NODE_EXTRA_CA_CERTS".into(),
                    "REQUESTS_CA_BUNDLE".into(),
                    "CURL_CA_BUNDLE".into(),
                ],
            })
            .await
            .expect("GetEnv request");

        let values = match resp {
            Response::GetEnv { values } => values,
            other => panic!("expected GetEnv response, got {other:?}"),
        };
        for (i, v) in values.iter().enumerate() {
            assert_eq!(
                v.as_deref(),
                Some("/etc/ssl/corp-ca.pem"),
                "index {i} should be the CA bundle path"
            );
        }
    }

    #[tokio::test]
    async fn spawn_with_policy_and_proxy_none_omits_env_vars() {
        use koda_sandbox::ipc::{Request, Response};
        use koda_sandbox::policy::SandboxPolicy;

        let root = TempDir::new().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();

        let mut client = WorkerClient::spawn_with_policy_and_proxy(
            canonical_root,
            &SandboxPolicy::default(),
            None,
        )
        .await
        .expect("spawn_with_policy_and_proxy");

        let resp = client
            .request(&Request::GetEnv {
                names: vec!["HTTPS_PROXY".into(), "NO_PROXY".into()],
            })
            .await
            .expect("GetEnv request");

        let values = match resp {
            Response::GetEnv { values } => values,
            other => panic!("expected GetEnv response, got {other:?}"),
        };
        // proxy=None must not inherit the host's HTTPS_PROXY either — we
        // only inject what we explicitly set. But std::env::var on the worker
        // side reads the worker's environment, which inherits ours. So this
        // assertion is conditional: skip if the host already had HTTPS_PROXY
        // set (e.g. behind a corp proxy at test time).
        if std::env::var("HTTPS_PROXY").is_err() {
            assert_eq!(values[0], None, "HTTPS_PROXY must not be injected");
        }
    }
}
