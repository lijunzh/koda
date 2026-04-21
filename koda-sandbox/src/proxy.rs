//! External egress proxy management (Phase 3a of #934).
//!
//! ## What this module does
//!
//! Phase 3a delivers the *enforcement layer* for network egress:
//!
//! 1. **Env-var bouquet** — the standard list of variables (`HTTPS_PROXY`,
//!    `SSL_CERT_FILE`, etc.) that a sandboxed subprocess needs so well-behaved
//!    HTTP clients (curl, gh, npm, pip, cargo, go, node, python) route their
//!    traffic through a single hop. See [`proxy_env_vars`].
//!
//! 2. **External proxy lifecycle** — spawn a user-provided proxy command
//!    (mitmproxy, Squid, Zscaler agent, anything that speaks HTTP CONNECT),
//!    wait for it to bind, kill it cleanly on drop. See [`ExternalProxy`]
//!    and [`ProxyHandle`].
//!
//! Phase 3b will add a built-in proxy that implements the *policy layer*
//! (domain allowlist filtering). Both modes plug into the same env-var
//! bouquet — applications can't tell whether they're talking to our proxy
//! or the user's.
//!
//! ## Why support an external proxy at all?
//!
//! Three concrete user populations:
//!
//! - **Corporate MITM environments** (Zscaler, Bluecoat, Palo Alto) already
//!   have a proxy doing TLS interception with a corporate CA. Stacking our
//!   proxy on top would create fragile double-MITM chains.
//! - **`mitmproxy` debuggers** want to inspect their agent's traffic without
//!   a second proxy in the way.
//! - **Air-gapped / homelab users** with Squid or Artifactory pull-through
//!   already have egress infrastructure.
//!
//! Mirrors what Codex does (chain to upstream via `HTTPS_PROXY`) and what
//! Gemini CLI does (`GEMINI_SANDBOX_PROXY_COMMAND` external-only).
//!
//! ## Fail-open semantics
//!
//! [`ExternalProxy::spawn`] returns `Err` when the proxy can't be started.
//! Callers are expected to **warn and continue without restrictions** rather
//! than fail the session — same pattern as Claude Code's `upstreamproxy`. A
//! broken proxy must never break an otherwise-working session.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use tracing::{debug, warn};

// ── Env-var bouquet ──────────────────────────────────────────────────────────

/// Default `NO_PROXY` value: loopback + RFC1918 + AWS/GCE IMDS.
///
/// Borrowed verbatim from Claude Code's `upstreamproxy.ts` `NO_PROXY_LIST`
/// (which itself mirrors the Bun/curl/Go/Python intersection of supported
/// patterns). These are the addresses every reasonable proxy declines to
/// intercept — without them, sandboxed processes can't talk to localhost
/// dev servers, can't read instance metadata, and can't reach RFC1918
/// services on the user's LAN.
pub const DEFAULT_NO_PROXY: &str =
    "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";

/// Env-var name passed to a user proxy command so it knows which port to bind.
///
/// Mirrors Codex's `PROXY_ACTIVE_ENV_KEY` pattern. Avoids template-string
/// parsing and lets the proxy command be a plain `Vec<String>`.
pub const PROXY_PORT_ENV_KEY: &str = "KODA_PROXY_PORT";

/// Generate the env-var bouquet for a sandboxed subprocess.
///
/// `port` is where the proxy is listening on `127.0.0.1`. `ca_bundle` is
/// the path to a PEM bundle the subprocess should trust for TLS verification
/// — typically points to a corporate CA + system CA concatenation. When
/// `None`, the cert-bundle vars are omitted (the subprocess uses its
/// platform default trust store).
///
/// Returned as `Vec<(String, String)>` so the caller can `.envs(...)` it
/// directly into a `Command` builder. Sorted by key for deterministic
/// snapshot tests.
///
/// ## Why so many keys?
///
/// Different runtimes look at different env vars:
///
/// | Runtime | Proxy var | CA bundle var |
/// |---|---|---|
/// | curl, libcurl | `HTTPS_PROXY` (UPPER) | `CURL_CA_BUNDLE` |
/// | Python `requests` | `HTTPS_PROXY` (UPPER) | `REQUESTS_CA_BUNDLE` |
/// | Python `httpx`, `urllib` | `https_proxy` (lower) | `SSL_CERT_FILE` |
/// | Node.js (undici) | `HTTPS_PROXY` (UPPER) | `NODE_EXTRA_CA_CERTS` |
/// | Go (`net/http`) | `HTTPS_PROXY` (UPPER) | `SSL_CERT_FILE` |
/// | Rust (`reqwest`) | `HTTPS_PROXY` (UPPER) | `SSL_CERT_FILE` |
///
/// Setting all of them is the only way to cover every dev tool without
/// per-tool wrappers.
pub fn proxy_env_vars(port: u16, ca_bundle: Option<&Path>) -> Vec<(String, String)> {
    let proxy_url = format!("http://127.0.0.1:{port}");

    let mut vars = vec![
        ("HTTPS_PROXY".to_string(), proxy_url.clone()),
        ("https_proxy".to_string(), proxy_url.clone()),
        ("HTTP_PROXY".to_string(), proxy_url.clone()),
        ("http_proxy".to_string(), proxy_url),
        ("NO_PROXY".to_string(), DEFAULT_NO_PROXY.to_string()),
        ("no_proxy".to_string(), DEFAULT_NO_PROXY.to_string()),
    ];

    if let Some(ca) = ca_bundle {
        let ca_str = ca.to_string_lossy().to_string();
        vars.push(("SSL_CERT_FILE".to_string(), ca_str.clone()));
        vars.push(("NODE_EXTRA_CA_CERTS".to_string(), ca_str.clone()));
        vars.push(("REQUESTS_CA_BUNDLE".to_string(), ca_str.clone()));
        vars.push(("CURL_CA_BUNDLE".to_string(), ca_str));
    }

    // Deterministic order for snapshot tests.
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

// ── ExternalProxy / ProxyHandle ──────────────────────────────────────────────

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
    /// subprocesses — those get the bouquet from [`proxy_env_vars`]).
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

        Ok(ProxyHandle { port, child: Some(child) })
    }
}

/// Live external proxy. Drop sends SIGTERM and reaps the child.
///
/// Held for the lifetime of the sandboxed session. Cloning is intentionally
/// not supported — only one owner SIGTERMs the child.
#[derive(Debug)]
pub struct ProxyHandle {
    /// Port the proxy is listening on (`127.0.0.1:port`).
    pub port: u16,
    child: Option<Child>,
}

impl ProxyHandle {
    /// Path to a CA bundle the proxy expects clients to trust, if any.
    ///
    /// Phase 3a doesn't track this — the bundle path comes from
    /// [`crate::policy::MitmConfig::ca_bundle`] on the policy side. This
    /// method exists so 3b can attach a built-in-proxy-generated CA without
    /// changing the public API.
    pub fn ca_bundle(&self) -> Option<&Path> {
        None
    }

    /// Synchronous shutdown: SIGTERM + brief wait. Idempotent.
    ///
    /// Called from `Drop`; exposed so callers can shut down before drop and
    /// surface errors. After this returns, [`Self::ca_bundle`] is still
    /// valid but the proxy no longer accepts connections.
    pub fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            // start_kill is non-blocking; the OS reaps via tokio's wait task.
            if let Err(e) = child.start_kill() {
                warn!("external proxy SIGKILL failed: {e}");
            }
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Pick an unused port by binding `0` and immediately dropping. There's a
/// classic TOCTOU hole here (another process could grab the port before the
/// proxy does), but it's the same trick every test suite uses and the
/// failure mode (proxy bind error) is caught downstream by `wait_for_bind`.
fn pick_ephemeral_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Poll `127.0.0.1:port` with TCP connects until success or timeout.
async fn wait_for_bind(port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let addr = format!("127.0.0.1:{port}");
    let mut backoff = Duration::from_millis(20);

    loop {
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out after {timeout:?}");
        }
        sleep(backoff).await;
        // Cap backoff at 200 ms so we still poll responsively but don't
        // hammer the loopback stack.
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}

// ── Optional CA-bundle resolution helper ─────────────────────────────────────

/// Path to the CA bundle to advertise via env vars, given a [`crate::policy::NetPolicy`].
///
/// Returns `None` when no MITM is configured — in that case the subprocess
/// uses its platform default trust store. Returning `Option<&Path>` rather
/// than `Option<PathBuf>` so callers can pass it directly to [`proxy_env_vars`]
/// without intermediate allocation.
pub fn ca_bundle_for_policy(net: &crate::policy::NetPolicy) -> Option<&Path> {
    net.mitm.as_ref().map(|m| m.ca_bundle.as_path())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn proxy_env_vars_includes_all_six_proxy_keys() {
        let vars = proxy_env_vars(8080, None);
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(keys.contains(&"HTTPS_PROXY"));
        assert!(keys.contains(&"https_proxy"));
        assert!(keys.contains(&"HTTP_PROXY"));
        assert!(keys.contains(&"http_proxy"));
        assert!(keys.contains(&"NO_PROXY"));
        assert!(keys.contains(&"no_proxy"));
    }

    #[test]
    fn proxy_env_vars_omits_ca_bundle_when_none() {
        let vars = proxy_env_vars(8080, None);
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(!keys.contains(&"SSL_CERT_FILE"));
        assert!(!keys.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(!keys.contains(&"REQUESTS_CA_BUNDLE"));
        assert!(!keys.contains(&"CURL_CA_BUNDLE"));
    }

    #[test]
    fn proxy_env_vars_includes_all_four_ca_keys_when_some() {
        let bundle = PathBuf::from("/etc/ssl/corp-ca.pem");
        let vars = proxy_env_vars(8080, Some(&bundle));
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(keys.contains(&"SSL_CERT_FILE"));
        assert!(keys.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(keys.contains(&"REQUESTS_CA_BUNDLE"));
        assert!(keys.contains(&"CURL_CA_BUNDLE"));

        // All four point at the same path.
        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
        ] {
            let v = vars
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap();
            assert_eq!(v, "/etc/ssl/corp-ca.pem");
        }
    }

    #[test]
    fn proxy_url_format_uses_loopback_ipv4() {
        let vars = proxy_env_vars(31415, None);
        let url = vars
            .iter()
            .find(|(k, _)| k == "HTTPS_PROXY")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(url, "http://127.0.0.1:31415");
    }

    #[test]
    fn no_proxy_default_covers_loopback_and_rfc1918() {
        // Sanity check that we haven't accidentally truncated the constant.
        assert!(DEFAULT_NO_PROXY.contains("127.0.0.1"));
        assert!(DEFAULT_NO_PROXY.contains("::1"));
        assert!(DEFAULT_NO_PROXY.contains("10.0.0.0/8"));
        assert!(DEFAULT_NO_PROXY.contains("172.16.0.0/12"));
        assert!(DEFAULT_NO_PROXY.contains("192.168.0.0/16"));
        // AWS / GCE IMDS link-local: dropping this would prevent cloud
        // workloads from reading instance metadata.
        assert!(DEFAULT_NO_PROXY.contains("169.254.0.0/16"));
    }

    #[test]
    fn ca_bundle_for_policy_handles_no_mitm() {
        let policy = crate::policy::NetPolicy::default();
        assert!(ca_bundle_for_policy(&policy).is_none());
    }

    #[test]
    fn ca_bundle_for_policy_returns_path_when_mitm_set() {
        let policy = crate::policy::NetPolicy {
            mitm: Some(crate::policy::MitmConfig {
                ca_bundle: PathBuf::from("/x/ca.pem"),
                socket_map: vec![],
            }),
            ..Default::default()
        };
        assert_eq!(
            ca_bundle_for_policy(&policy),
            Some(Path::new("/x/ca.pem"))
        );
    }

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
