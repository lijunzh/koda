//! User-provided egress proxy lifecycle (Phase 3a of #934).
//!
//! See [parent module docs](super) for the why.

use super::{PROXY_PORT_ENV_KEY, ProxyHandle, pick_ephemeral_port, wait_for_bind};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

/// Spec for a user-provided egress proxy.
///
/// The proxy must:
/// - Bind on `127.0.0.1:$KODA_PROXY_PORT` (port supplied via env var; see
///   [`PROXY_PORT_ENV_KEY`]).
/// - Speak HTTP CONNECT on that port.
/// - Stay running until killed.
///
/// Use [`ExternalProxy::spawn`] to start one.
#[derive(Debug, Clone)]
pub struct ExternalProxy {
    /// Argv. The first element is the program; the rest are arguments.
    /// `KODA_PROXY_PORT` is injected into the env, not interpolated into args
    /// — proxies that need it on the command line should reference the env
    /// var via shell or accept a `--port` flag from their own wrapper script.
    pub command: Vec<String>,

    /// Extra env vars for the proxy process itself (not for sandboxed
    /// subprocesses — those get the bouquet from
    /// [`crate::proxy::proxy_env_vars`]).
    pub env: HashMap<String, String>,

    /// Bind port. `None` selects an ephemeral port.
    ///
    /// ## Why explicit port support
    ///
    /// Some proxy implementations (notably `mitmdump`'s default config and
    /// many corporate Zscaler agents) require a fixed port advertised to
    /// other infrastructure. Ephemeral is the safer default.
    pub port: Option<u16>,

    /// Maximum time to wait for the proxy to bind. Default 5 s (matches CC's
    /// CA-fetch timeout — generous for local startup, snappy enough that a
    /// hung proxy doesn't block the whole session).
    pub startup_timeout: Duration,
}

impl ExternalProxy {
    /// Construct with sensible defaults. `command[0]` is the program.
    pub fn new<S: Into<String>>(command: impl IntoIterator<Item = S>) -> Self {
        Self {
            command: command.into_iter().map(Into::into).collect(),
            env: HashMap::new(),
            port: None,
            startup_timeout: Duration::from_secs(5),
        }
    }

    /// Spawn the proxy and wait for it to bind.
    ///
    /// Returns a [`ProxyHandle`] whose `Drop` impl SIGTERMs the child. On
    /// failure (bad command, bind timeout, etc.), the caller should `warn!`
    /// and continue **without** routing traffic through anything — that's
    /// the fail-open contract.
    pub async fn spawn(&self) -> Result<ProxyHandle> {
        if self.command.is_empty() {
            bail!("ExternalProxy::command must not be empty");
        }

        // Reserve a port up front. Binding it ourselves and then immediately
        // dropping the listener would race with the proxy binding; so we just
        // pick one if unspecified and trust the proxy to grab it.
        let port = match self.port {
            Some(p) => p,
            None => pick_ephemeral_port().context("pick ephemeral port for proxy")?,
        };

        let mut cmd = Command::new(&self.command[0]);
        cmd.args(&self.command[1..]);
        cmd.env(PROXY_PORT_ENV_KEY, port.to_string());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // Detach stdin so the proxy can't read from our terminal.
        cmd.stdin(std::process::Stdio::null());

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn external proxy: {:?}", self.command))?;

        debug!(
            "external proxy spawned: cmd={:?} port={} pid={:?}",
            self.command,
            port,
            child.id()
        );

        // Poll the port until it accepts a connection or we time out.
        wait_for_bind(port, self.startup_timeout)
            .await
            .with_context(|| format!("external proxy did not bind 127.0.0.1:{port}"))?;

        Ok(ProxyHandle::from_child(port, child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_proxy_new_sets_defaults() {
        let p = ExternalProxy::new(["mitmdump", "--listen-port", "8877"]);
        assert_eq!(p.command, vec!["mitmdump", "--listen-port", "8877"]);
        assert!(p.env.is_empty());
        assert!(p.port.is_none());
        assert_eq!(p.startup_timeout, Duration::from_secs(5));
    }

    #[tokio::test]
    async fn external_proxy_empty_command_errors() {
        let p = ExternalProxy {
            command: vec![],
            env: HashMap::new(),
            port: None,
            startup_timeout: Duration::from_millis(100),
        };
        let err = p.spawn().await.expect_err("must error on empty command");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn external_proxy_unbound_command_times_out() {
        // `true` (the unix command) exits immediately and never binds a port.
        // Should fail with a "did not bind" context.
        let p = ExternalProxy {
            command: vec!["true".to_string()],
            env: HashMap::new(),
            port: None,
            startup_timeout: Duration::from_millis(150),
        };
        let err = p.spawn().await.expect_err("must time out");
        let msg = format!("{err:#}");
        assert!(msg.contains("did not bind"), "got: {msg}");
    }
}
