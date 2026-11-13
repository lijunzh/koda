//! `initialize`: the handshake that opens every connection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The client says who it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The client's name and version.
    pub client_info: ClientInfo,
}

/// A client's name and version, such as `koda-cli` `0.7.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    /// The client's name.
    pub name: String,
    /// The client's version.
    pub version: String,
}

/// The server says what it is and which protocol it speaks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// Koda's name and version.
    pub server_info: ServerInfo,
    /// [`crate::PROTOCOL_VERSION`] of the server.
    pub protocol_version: u32,
}

/// Koda's name and version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    /// Always `koda`.
    pub name: String,
    /// The Koda release, such as `0.6.4`.
    pub version: String,
}
