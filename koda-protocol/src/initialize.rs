//! `initialize`: the handshake that opens every connection.
//!
//! As Codex's: the client says who it is and what it takes
//! (`capabilities`): `experimentalApi` opts into the methods and
//! notifications still marked experimental, and `optOutNotificationMethods`
//! names notifications this connection never wants. Both hold for the
//! connection that sent them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The client says who it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The client's name and version.
    pub client_info: ClientInfo,
    /// What the client takes. Without them, nothing experimental and every
    /// notification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<InitializeCapabilities>,
}

/// A client's name and version, such as `koda-cli` `0.7.0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClientInfo {
    /// The client's name.
    pub name: String,
    /// A name to show people, such as "Koda Desktop", as Codex's
    /// `clientInfo.title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The client's version.
    pub version: String,
}

/// What a client takes, said in `initialize`: Codex's
/// `InitializeCapabilities`, the two Koda has a use for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    /// Opt into the methods and notifications still marked experimental
    /// ([`EXPERIMENTAL_METHODS`], [`EXPERIMENTAL_NOTIFICATIONS`]). Without
    /// it, such a method is refused and such a notification isn't sent.
    #[serde(default)]
    pub experimental_api: bool,
    /// Notifications this connection never wants, by exact method name
    /// (such as `item/agentMessage/delta`). Requests and responses are
    /// always sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_out_notification_methods: Option<Vec<String>>,
}

/// Methods a client may call only with `experimentalApi`: new ones start
/// here, as Codex marks its own, and leave once settled. None yet.
pub const EXPERIMENTAL_METHODS: &[&str] = &[];

/// Notifications sent only to clients with `experimentalApi`. None yet.
pub const EXPERIMENTAL_NOTIFICATIONS: &[&str] = &[];

/// Why `method` needs `experimentalApi`, if it does: Codex's
/// `experimental_reason`, by method.
pub fn experimental_reason(method: &str) -> Option<&'static str> {
    EXPERIMENTAL_METHODS
        .iter()
        .find(|experimental| **experimental == method)
        .copied()
}

/// The refusal of an experimental method or field to a client without
/// `experimentalApi`, in Codex's words.
pub fn experimental_required_message(reason: &str) -> String {
    format!("{reason} requires experimentalApi capability")
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
    /// The Koda release, such as `0.7.0`.
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_read_as_codex_sends_them() {
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "clientInfo": {"name": "app", "title": "Koda Desktop", "version": "1"},
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": ["item/agentMessage/delta"],
            },
        }))
        .unwrap();
        assert_eq!(params.client_info.title.as_deref(), Some("Koda Desktop"));
        let capabilities = params.capabilities.unwrap();
        assert!(capabilities.experimental_api);
        assert_eq!(
            capabilities.opt_out_notification_methods,
            Some(vec!["item/agentMessage/delta".to_string()])
        );
        // An older client's, with neither, still reads.
        let older: InitializeParams = serde_json::from_value(serde_json::json!({
            "clientInfo": {"name": "tui", "version": "0.7.12"},
        }))
        .unwrap();
        assert_eq!(older.capabilities, None);
        assert_eq!(older.client_info.title, None);
    }

    #[test]
    fn an_experimental_refusal_is_worded_as_codex_words_it() {
        assert_eq!(
            experimental_required_message("thread/x"),
            "thread/x requires experimentalApi capability"
        );
        assert_eq!(experimental_reason("session/start"), None);
    }
}
