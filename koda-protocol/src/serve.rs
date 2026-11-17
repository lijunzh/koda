//! The shared server itself (`koda serve`): stopping it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `serve/stop`: stop the shared server, as `claude daemon stop` stops
/// Claude Code's supervisor. Only `koda serve` takes it; a host in the
/// client's process refuses it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServeStopParams {
    /// Stop only if nothing would be lost: no open session and no other
    /// client. A TUI that finds a server of another version asks this way.
    #[serde(default)]
    pub if_idle: bool,
}

/// The answer to `serve/stop`, sent before the server stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServeStopResponse {
    /// Whether it is stopping.
    pub stopped: bool,
    /// Why it isn't, when it isn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
