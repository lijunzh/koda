//! Polymorphic lifecycle wrapper for any spawned proxy (Phase 3a of #934).
//!
//! Today there's exactly one variant — a child process started by
//! [`crate::proxy::ExternalProxy`]. Phase 3b will add a built-in variant
//! whose lifecycle is a tokio task instead of a child process. Both share
//! the same public surface so callers (most importantly
//! [`crate::worker_client::WorkerClient::spawn_with_policy_and_proxy`]) can
//! store either kind in the same field without trait objects.
//!
//! See [parent module docs](super) for the broader why.

use std::path::Path;
use tokio::process::Child;
use tracing::warn;

/// Live proxy. `Drop` cleans up the underlying resource (SIGTERM for child
/// processes; abort for tokio tasks once 3b lands).
///
/// Cloning is intentionally not supported — only one owner shuts down the
/// underlying resource.
#[derive(Debug)]
pub struct ProxyHandle {
    /// Port the proxy is listening on (`127.0.0.1:port`).
    pub port: u16,
    /// Backing resource. Boxed-enum once 3b adds the built-in variant; for
    /// now the single child-process variant lives inline.
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    /// Child process spawned by [`crate::proxy::ExternalProxy`].
    /// `None` after [`ProxyHandle::shutdown`] has been called.
    External(Option<Child>),
}

impl ProxyHandle {
    /// Construct from a child process. Crate-private constructor; callers
    /// reach this through [`crate::proxy::ExternalProxy::spawn`].
    pub(crate) fn from_child(port: u16, child: Child) -> Self {
        Self {
            port,
            inner: Inner::External(Some(child)),
        }
    }

    /// Path to a CA bundle the proxy expects clients to trust, if any.
    ///
    /// External proxies always return `None` — the bundle path comes from
    /// [`crate::policy::MitmConfig::ca_bundle`] on the policy side. The 3b
    /// built-in proxy variant will return `Some(generated_ca_path)`.
    pub fn ca_bundle(&self) -> Option<&Path> {
        match &self.inner {
            Inner::External(_) => None,
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
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
