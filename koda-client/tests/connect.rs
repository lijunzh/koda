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
