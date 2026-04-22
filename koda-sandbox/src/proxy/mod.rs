//! External egress proxy management (Phase 3a of #934).
//!
//! ## What this module does
//!
//! Phase 3a delivered the *enforcement layer* for network egress:
//!
//! 1. **Env-var bouquet** ([`mod@env`]) — the standard list of variables
//!    (`HTTPS_PROXY`, `SSL_CERT_FILE`, etc.) that a sandboxed subprocess
//!    needs so well-behaved HTTP clients (curl, gh, npm, pip, cargo, go,
//!    node, python) route their traffic through a single hop.
//!
//! 2. **External proxy lifecycle** ([`external`]) — spawn a user-provided
//!    proxy command (mitmproxy, Squid, Zscaler agent, anything that speaks
//!    HTTP CONNECT), wait for it to bind, kill it cleanly on drop.
//!
//! 3. **`ProxyHandle`** ([`handle`]) — the polymorphic lifecycle wrapper
//!    every spawn path returns, so callers can store any proxy variant
//!    behind one type without trait objects or enums on the hot path.
//!
//! Phase 3b adds the built-in proxy ([`builtin`]) that implements
//! the *policy layer* (domain allowlist filtering). All variants plug
//! into the same env-var bouquet and the same [`ProxyHandle`] type —
//! applications can't tell whether they're talking to our proxy or
//! the user's, and the slot manager doesn't care which spawn path was
//! used to create the handle.
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
//!
//! ## Module layout
//!
//! Split per concern in commit-1 of Phase 3b so future additions don't push
//! any single file past 600 lines:
//!
//! ```text
//! proxy/
//! ├── mod.rs            — this docstring + shared internal helpers
//! ├── env.rs            — env-var bouquet + DEFAULT_NO_PROXY (3a) + socks5 (3d)
//! ├── external.rs       — ExternalProxy::spawn (3a)
//! ├── handle.rs         — ProxyHandle (External + BuiltIn variants)
//! ├── filter.rs         — hostname allowlist (3b)
//! ├── server.rs         — HTTP CONNECT proxy (3b)
//! ├── builtin.rs        — BuiltInProxy::spawn (3b)
//! ├── socks5.rs         — SOCKS5 server (3d)
//! ├── builtin_socks5.rs — BuiltInSocks5Proxy::spawn (3d)
//! └── upstream.rs       — corp HTTPS_PROXY chaining (3d.3)
//! ```

pub mod builtin;
pub mod builtin_socks5;
pub mod env;
pub mod external;
pub mod filter;
pub mod handle;
pub mod relay;
pub mod server;
pub mod socks5;
pub mod upstream;

pub use builtin::BuiltInProxy;
pub use builtin_socks5::BuiltInSocks5Proxy;
pub use env::{
    DEFAULT_NO_PROXY, PROXY_PORT_ENV_KEY, ca_bundle_for_policy, proxy_env_vars, socks5_env_vars,
};
pub use external::ExternalProxy;
pub use filter::{DEFAULT_DEV_ALLOWLIST, Filter};
pub use handle::ProxyHandle;
pub use server::Server;
pub use socks5::Socks5Server;
pub use upstream::UpstreamConfig;

// ── Shared internal helpers ──────────────────────────────────────────────────
//
// These two helpers are needed by every spawn path (today: `ExternalProxy`;
// 3b: `BuiltInProxy`). Living in `mod.rs` as `pub(super)` keeps them close
// to the docstring that explains why they exist while still being reachable
// from sibling modules.

use anyhow::{Result, bail};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep};

/// Pick an unused port by binding `0` and immediately dropping.
///
/// There's a classic TOCTOU hole here (another process could grab the port
/// before the proxy does), but it's the same trick every test suite uses
/// and the failure mode (proxy bind error) is caught downstream by
/// [`wait_for_bind`].
pub(crate) fn pick_ephemeral_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Poll `127.0.0.1:port` with TCP connects until success or timeout.
///
/// Used to confirm a freshly-spawned proxy has actually bound its socket
/// before we start handing its env vars to subprocesses. Exponential
/// backoff capped at 200 ms so we don't hammer the loopback stack.
pub(crate) async fn wait_for_bind(port: u16, timeout: Duration) -> Result<()> {
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
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}
