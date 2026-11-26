//! [`koda_client::InProcessClient::connect`]: the same client on the shared
//! server's socket instead of a host in the process.
#![cfg(unix)]

use std::time::{Duration, Instant};

use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::methods;
use koda_client::socket::{Options, serve_with_config};
use koda_client::stdio::ServeArgs;
use koda_client::{ConfigFile, InProcessClient};
use koda_core::config::{KodaConfig, ProviderType};

#[tokio::test(flavor = "multi_thread")]
async fn the_client_connects_to_the_shared_server() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let socket = dir.path().join("serve").join("koda.sock");
    let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    config.base_url = "http://127.0.0.1:9/v1".into();
    config.model = "stub".into();
    config.model_settings.model = "stub".into();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(serve_with_config(
        ServeArgs {
            config_file: ConfigFile::in_dir(&dir.path().join("config")),
            project_root: project,
            home: dir.path().join("config"),
        },
        config,
        socket.clone(),
        Options::default(),
        async {
            let _ = stopped.await;
        },
    ));
    let info = ClientInfo {
        name: "connect-test".into(),
        version: "0".into(),
        title: None,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let client = loop {
        match InProcessClient::connect(&socket, dir.path().to_path_buf(), info.clone()).await {
            Ok(client) => break client,
            Err(e) => {
                assert!(Instant::now() < deadline, "never connected: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    assert_eq!(client.server().server_info.name, "koda");
    let started = client
        .request::<methods::SessionStart>(Default::default())
        .await
        .unwrap();
    assert!(!started.session.id.is_empty());
    client.shutdown().await;

    // No server: a clear error, not a hang.
    let missing = InProcessClient::connect(
        &dir.path().join("nothing.sock"),
        dir.path().to_path_buf(),
        info,
    )
    .await;
    assert!(
        missing
            .as_ref()
            .is_err_and(|e| e.to_string().contains("couldn't reach koda serve")),
        "{:?}",
        missing.err()
    );
    let _ = stop.send(());
    served.await.unwrap().unwrap();
}

/// The shared server at `socket` with a stub model, until `stopped`.
fn serve(
    dir: &std::path::Path,
    socket: &std::path::Path,
    stopped: tokio::sync::oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<Result<(), koda_client::socket::ServeError>> {
    let mut config = KodaConfig::default_for_testing(ProviderType::LMStudio);
    config.base_url = "http://127.0.0.1:9/v1".into();
    config.model = "stub".into();
    config.model_settings.model = "stub".into();
    let project = dir.join("project");
    std::fs::create_dir_all(&project).unwrap();
    tokio::spawn(serve_with_config(
        ServeArgs {
            config_file: ConfigFile::in_dir(&dir.join("config")),
            project_root: project,
            home: dir.join("config"),
        },
        config,
        socket.to_path_buf(),
        Options::default(),
        async {
            let _ = stopped.await;
        },
    ))
}

/// A client whose server stops gets its session back from the server that
/// replaces it, on Codex's reconnect schedule: the first tries find nothing,
/// a later one connects and opens the same session.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_reconnects_to_a_restarted_server() {
    use koda_client::protocol::session::SessionStartParams;
    use koda_client::reconnect::{BUDGET, Failure, reconnect, worth_retrying};

    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("serve").join("koda.sock");
    let info = ClientInfo {
        name: "reconnect-test".into(),
        version: "0".into(),
        title: None,
    };
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let first = serve(dir.path(), &socket, stopped);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut client = loop {
        match InProcessClient::connect(&socket, dir.path().to_path_buf(), info.clone()).await {
            Ok(client) => break client,
            Err(e) => {
                assert!(Instant::now() < deadline, "never connected: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let session_id = client
        .request::<methods::SessionStart>(Default::default())
        .await
        .unwrap()
        .session
        .id;

    // The server goes away: the client's events end.
    let _ = stop.send(());
    first.await.unwrap().unwrap();
    while client.next_event().await.is_some() {}

    // Another starts on the same socket a moment later.
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let second = {
        let dir = dir.path().to_path_buf();
        let socket = socket.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            serve(&dir, &socket, stopped).await.unwrap()
        })
    };

    let tries = std::sync::atomic::AtomicUsize::new(0);
    let (client, reopened) = reconnect(BUDGET, || {
        tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let info = info.clone();
        let socket = socket.clone();
        let session_id = session_id.clone();
        let root = dir.path().to_path_buf();
        async move {
            let sorted = |e: koda_client::RequestError| {
                if worth_retrying(&e) {
                    Failure::Retry(e)
                } else {
                    Failure::Stop(e)
                }
            };
            let client = InProcessClient::connect(&socket, root, info)
                .await
                .map_err(sorted)?;
            let opened = client
                .request::<methods::SessionStart>(SessionStartParams {
                    session_id: Some(session_id),
                    ..Default::default()
                })
                .await
                .map_err(sorted)?;
            Ok((client, opened))
        }
    })
    .await
    .unwrap_or_else(|e| panic!("didn't reconnect: {e:?}"));
    assert_eq!(reopened.session.id, session_id);
    assert!(
        tries.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "the first try found no server"
    );

    client.shutdown().await;
    let _ = stop.send(());
    second.await.unwrap().unwrap();
}
