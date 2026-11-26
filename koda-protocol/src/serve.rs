//! The shared server itself (`koda serve`): stopping it, and making way for
//! a newer Koda.
//!
//! Every Koda client and the server run the same version. A client that
//! finds an older server asks it to make way (`serve/stop` with
//! `forUpdate`), and starts a server of its own version once it has gone; a
//! client that finds a newer one doesn't use it. Running turns are the only
//! work that holds the server back: the client asks its user whether to
//! wait for them (`afterTurns`) or stop them. The server says it is going
//! (`serve/updating`) before it does, so an older client doesn't start a
//! server of its own version in its place. Codex's app-server drains in
//! the same way when its manager restarts it onto a new version.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `serve/stop`: stop the shared server, as `claude daemon stop` stops
/// Claude Code's supervisor. Only `koda serve` takes it; a host in the
/// client's process refuses it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServeStopParams {
    /// Stop only if nothing would be lost. Without `forUpdate`: no session
    /// with a client, a running turn or a waiting request, and no other
    /// client. With it: no running turn and no waiting request.
    #[serde(default)]
    pub if_idle: bool,
    /// The newer Koda version this server makes way for. Clients still
    /// connected don't hold it back, since none of them can go on with a
    /// server of another version; each is told with `serve/updating`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_update: Option<String>,
    /// With `forUpdate`: when turns are running, let them finish and stop
    /// after the last one, refusing new turns meanwhile, as Codex's
    /// app-server drains. Without it and without `ifIdle`, running turns
    /// are stopped and end interrupted.
    #[serde(default)]
    pub after_turns: bool,
}

/// The answer to `serve/stop`, sent before the server stops.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServeStopResponse {
    /// Whether it is stopping now.
    pub stopped: bool,
    /// Why it isn't, when it isn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Sessions with a turn running or a request waiting, when asked with
    /// `forUpdate`: what a client asks its user about. Servers before
    /// 0.7.13 never set it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub working: u32,
    /// Whether it is waiting for those turns to finish before it stops.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draining: bool,
}

/// `serve/updating`: the server is stopping to make way for a newer Koda,
/// now or once `working` sessions' turns finish, which it waits for
/// refusing new turns. Sent to every client. A client of the server's
/// version can't go on with the newer one: it tells its user to start it
/// again rather than reconnecting. Koda's own; Codex's clients learn of a
/// restart from the error a draining server answers new turns with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServeUpdatingNotification {
    /// The version that replaces it.
    pub version: String,
    /// Sessions whose turns it waits for; 0 when it stops at once.
    pub working: u32,
}

/// The code and `data.reason` of the error a draining server answers a new
/// turn with, as Codex's: `{"reason": "serverShuttingDown"}`.
pub const DRAINING_REASON: &str = "serverShuttingDown";

/// The message of that error, Codex's.
pub const DRAINING_MESSAGE: &str = "Server is draining; retry after reconnecting";

fn is_zero(n: &u32) -> bool {
    *n == 0
}
