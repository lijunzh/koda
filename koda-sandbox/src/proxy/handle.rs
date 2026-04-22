//! Polymorphic lifecycle wrapper for any spawned proxy (Phase 3a/3b of #934).
//!
//! Two variants today:
//!
//! - **External** — a child process started by
//!   [`crate::proxy::ExternalProxy`] (Phase 3a). Lifecycle = `Child`.
//! - **BuiltIn** — an in-process tokio task spawned by
//!   [`crate::proxy::BuiltInProxy`] (Phase 3b). Lifecycle = `JoinHandle`.
//!
//! Both share the same public surface so callers (most importantly
//! [`crate::worker_client::WorkerClient::spawn_with_policy_and_proxy`]) can
//! store either kind in the same field without trait objects or visible
//! enums on the hot path.
//!
//! See [parent module docs](super) for the broader why.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use tokio::process::Child;
use tokio::task::JoinHandle;
use tracing::warn;

/// Live proxy. `Drop` cleans up the underlying resource (SIGTERM for
/// child processes; `abort()` for tokio tasks).
///
/// Cloning is intentionally not supported — only one owner shuts down
/// the underlying resource.
#[derive(Debug)]
pub struct ProxyHandle {
    /// Port the proxy is listening on (`127.0.0.1:port`).
    pub port: u16,
    /// Backing resource. Boxed-enum so callers don't need to match.
    inner: Inner,
    /// Linux-only side-channel: the UDS path of a tokio task that
    /// bridges Unix-socket accepts to the proxy's TCP port. Spawned
    /// alongside the TCP listener by
    /// [`crate::proxy::BuiltInProxy::spawn`] so the bwrap kernel-enforced
    /// egress path (Phase 3c.1) has something the sandbox can
    /// `connect()` through after `--unshare-net` strips all networking.
    ///
    /// `None` on macOS, on Linux when the bridge failed to bind, and
    /// for [`crate::proxy::ExternalProxy`] (which doesn't need the
    /// in-process bridge — the user already has a proxy elsewhere).
    /// `Some` on Linux for the in-process built-in proxy.
    ///
    /// `Drop` removes the file (best-effort) and aborts the task.
    #[cfg(target_os = "linux")]
    uds_bridge: Option<UdsBridge>,
}

/// Linux-only bridge bookkeeping. Kept inline (not its own module) since
/// it's a tiny pair of fields and only used here.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct UdsBridge {
    path: PathBuf,
    task: JoinHandle<()>,
}

#[derive(Debug)]
enum Inner {
    /// Child process spawned by [`crate::proxy::ExternalProxy`].
    /// `None` after [`ProxyHandle::shutdown`] has been called.
    External(Option<Child>),
    /// Tokio task spawned by [`crate::proxy::BuiltInProxy`].
    /// `None` after [`ProxyHandle::shutdown`] has been called.
    BuiltIn(Option<JoinHandle<()>>),
}

impl ProxyHandle {
    /// Construct from a child process. Crate-private constructor; callers
    /// reach this through [`crate::proxy::ExternalProxy::spawn`].
    pub(crate) fn from_child(port: u16, child: Child) -> Self {
        Self {
            port,
            inner: Inner::External(Some(child)),
            #[cfg(target_os = "linux")]
            uds_bridge: None,
        }
    }

    /// Construct from a tokio task. Crate-private constructor; callers
    /// reach this through [`crate::proxy::BuiltInProxy::spawn`].
    pub(crate) fn from_task(port: u16, task: JoinHandle<()>) -> Self {
        Self {
            port,
            inner: Inner::BuiltIn(Some(task)),
            #[cfg(target_os = "linux")]
            uds_bridge: None,
        }
    }

    /// Attach a Linux-only UDS bridge spawned by
    /// [`crate::bwrap_proxy::spawn_uds_bridge`]. The handle takes
    /// ownership of the task + path, abort-and-unlinks them on drop.
    ///
    /// Crate-private — only `BuiltInProxy::spawn` is meant to call
    /// this, immediately after spawning both the TCP listener and the
    /// bridge.
    #[cfg(target_os = "linux")]
    pub(crate) fn with_uds_bridge(mut self, path: PathBuf, task: JoinHandle<()>) -> Self {
        self.uds_bridge = Some(UdsBridge { path, task });
        self
    }

    /// Path of the UDS bridge if one was attached. Used by
    /// [`crate::BwrapRuntime`] (Phase 3c.1.d) to know what to bind-mount
    /// into the sandbox.
    ///
    /// Returns `None` on macOS and for ExternalProxy.
    #[cfg(target_os = "linux")]
    pub fn uds_path(&self) -> Option<&Path> {
        self.uds_bridge.as_ref().map(|b| b.path.as_path())
    }

    /// Path to a CA bundle the proxy expects clients to trust, if any.
    ///
    /// External proxies always return `None` — the bundle path comes from
    /// [`crate::policy::MitmConfig::ca_bundle`] on the policy side.
    /// Built-in proxies also return `None` in 3b (no MITM); Phase 3d will
    /// swap this to `Some(generated_ca_path)` for the built-in MITM mode.
    pub fn ca_bundle(&self) -> Option<&Path> {
        match &self.inner {
            Inner::External(_) | Inner::BuiltIn(_) => None,
        }
    }

    /// Synchronous shutdown: SIGTERM (or task-abort) + brief wait. Idempotent.
    ///
    /// Called from `Drop`; exposed so callers can shut down before drop and
    /// surface errors. After this returns, [`Self::ca_bundle`] is still
    /// valid but the proxy no longer accepts connections.
    pub fn shutdown(&mut self) {
        match &mut self.inner {
            Inner::External(slot) => {
                if let Some(mut child) = slot.take() {
                    // start_kill is non-blocking; the OS reaps via tokio's wait task.
                    if let Err(e) = child.start_kill() {
                        warn!("external proxy SIGKILL failed: {e}");
                    }
                }
            }
            Inner::BuiltIn(slot) => {
                if let Some(task) = slot.take() {
                    // abort() is fire-and-forget; the runtime will run the
                    // task's destructors at the next await point. There's
                    // no failure mode here — even an already-finished task
                    // accepts abort() as a no-op.
                    task.abort();
                }
            }
        }
        // Linux-only: tear down the UDS bridge alongside. Order doesn't
        // matter — if a sandboxed client is mid-CONNECT through the
        // bridge, both halves dying simultaneously yields a clean EOF.
        #[cfg(target_os = "linux")]
        if let Some(bridge) = self.uds_bridge.take() {
            bridge.task.abort();
            crate::bwrap_proxy::cleanup_uds_path(&bridge.path);
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
