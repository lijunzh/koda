//! MCP servers on the host: `mcpServerStatus/list`, `config/mcpServer/reload`,
//! the `mcp_servers.<name>` config key and the startup notification, in
//! Codex's shapes ([`koda_protocol::mcp`]). Each session runs its own
//! servers ([`koda_core::mcp::McpManager`]); the config file keeps the
//! configured ones ([`crate::config_file`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use koda_core::config::KodaConfig;
use koda_core::mcp::McpServerConfig;
use koda_core::mcp::client::McpClientStatus;
use koda_core::mcp::config::{
    self as core_config, DEFAULT_STARTUP_TIMEOUT_SEC, DEFAULT_TOOL_TIMEOUT_SEC, McpTransport,
};
use koda_core::mcp::manager::{McpStartupStatus, McpStatusFn, SharedMcpManager};
use koda_protocol::ServerNotification;
use koda_protocol::mcp::{
    ListMcpServerStatusParams, ListMcpServerStatusResponse, McpServerConfigValue,
    McpServerConnectionStatus, McpServerStartupState, McpServerStatus,
    McpServerStatusUpdatedNotification, McpTool, SERVERS_KEY,
};
use koda_protocol::rpc::ErrorObject;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::events::Outgoing;
use crate::session::invalid;

/// The servers a session on this host starts: the config file's and this
/// run's `--mcp-config` ones. A server in the file that can't run is
/// skipped (the host warned about it after `initialize`).
pub(crate) fn configured(
    file: &koda_config::ConfigFile,
    config: &KodaConfig,
) -> Result<HashMap<String, McpServerConfig>, ErrorObject> {
    let loaded = crate::config_file::load(file)?;
    let configured = crate::config_file::servers(&loaded)
        .into_iter()
        .filter_map(|(name, value)| match to_core(&name, value) {
            Ok(server) => Some((name, server)),
            Err(e) => {
                tracing::warn!("skipping MCP server: {}", e.message);
                None
            }
        })
        .collect();
    Ok(koda_core::mcp::McpManager::configured(
        configured,
        &config.session_mcp_servers,
    ))
}

/// If `key_path` is `mcp_servers.<name>`, the name.
pub(crate) fn server_key(key_path: &str) -> Option<&str> {
    key_path
        .strip_prefix(SERVERS_KEY)
        .and_then(|rest| rest.strip_prefix('.'))
}

/// Check an `mcp_servers.<name>` edit as `koda mcp add` checks a server:
/// the server to save, or `None` to remove it.
pub(crate) fn check_edit(name: &str, value: Value) -> Result<Option<McpServerConfig>, ErrorObject> {
    core_config::validate_server_name(name).map_err(|e| invalid(format!("{e:#}")))?;
    match value {
        Value::Null => Ok(None),
        value => {
            let value: McpServerConfigValue = serde_json::from_value(value)
                .map_err(|e| invalid(format!("{SERVERS_KEY}.{name}: {e}")))?;
            to_core(name, value).map(Some)
        }
    }
}

/// A server in the config's shape as the engine keeps it.
fn to_core(name: &str, value: McpServerConfigValue) -> Result<McpServerConfig, ErrorObject> {
    let McpServerConfigValue {
        command,
        args,
        env,
        cwd,
        url,
        bearer_token_env_var,
        http_headers,
        startup_timeout_sec,
        tool_timeout_sec,
        required,
        enabled_tools,
        disabled_tools,
    } = value;
    let refuse = |why: &str| Err(invalid(format!("{SERVERS_KEY}.{name}: {why}")));
    let transport = match (command, url) {
        (Some(command), None) => {
            if bearer_token_env_var.is_some() || !http_headers.is_empty() {
                return refuse(
                    "`bearer_token_env_var` and `http_headers` are for a server with a `url`",
                );
            }
            McpTransport::Stdio {
                command,
                args,
                env: env.into_iter().collect(),
                cwd,
            }
        }
        (None, Some(url)) => {
            if !args.is_empty() || !env.is_empty() || cwd.is_some() {
                return refuse("`args`, `env` and `cwd` are for a server with a `command`");
            }
            McpTransport::Http {
                url,
                bearer_token_env_var,
                headers: http_headers.into_iter().collect(),
            }
        }
        (Some(_), Some(_)) => return refuse("give a `command` or a `url`, not both"),
        (None, None) => return refuse("give a `command` or a `url`"),
    };
    let config = McpServerConfig {
        transport,
        startup_timeout_sec: startup_timeout_sec.unwrap_or(DEFAULT_STARTUP_TIMEOUT_SEC),
        tool_timeout_sec: tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT_SEC),
        enabled_tools,
        disabled_tools,
        required,
    };
    config
        .validate()
        .map_err(|e| invalid(format!("{SERVERS_KEY}.{name}: {e:#}")))?;
    Ok(config)
}

/// A server as the engine keeps it, in the config's shape. Default
/// timeouts are left out.
pub(crate) fn to_value(config: &McpServerConfig) -> McpServerConfigValue {
    let mut value = McpServerConfigValue {
        startup_timeout_sec: (config.startup_timeout_sec != DEFAULT_STARTUP_TIMEOUT_SEC)
            .then_some(config.startup_timeout_sec),
        tool_timeout_sec: (config.tool_timeout_sec != DEFAULT_TOOL_TIMEOUT_SEC)
            .then_some(config.tool_timeout_sec),
        required: config.required,
        enabled_tools: config.enabled_tools.clone(),
        disabled_tools: config.disabled_tools.clone(),
        ..McpServerConfigValue::default()
    };
    match &config.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            value.command = Some(command.clone());
            value.args = args.clone();
            value.env = env.clone().into_iter().collect();
            value.cwd = cwd.clone();
        }
        McpTransport::Http {
            url,
            bearer_token_env_var,
            headers,
        } => {
            value.url = Some(url.clone());
            value.bearer_token_env_var = bearer_token_env_var.clone();
            value.http_headers = headers.clone().into_iter().collect();
        }
    }
    value
}

/// `mcpServerStatus/list` for an open session's servers.
pub(crate) async fn session_status(
    mcp: &SharedMcpManager,
    params: &ListMcpServerStatusParams,
) -> ListMcpServerStatusResponse {
    let manager = mcp.read().await;
    let mut data: Vec<McpServerStatus> = manager
        .status_summary()
        .into_iter()
        .filter(|s| wanted(params, &s.name))
        .map(|s| McpServerStatus {
            runtime_status: match s.status {
                McpClientStatus::Connecting => McpServerConnectionStatus::Starting,
                McpClientStatus::Connected => McpServerConnectionStatus::Connected,
                McpClientStatus::Failed => McpServerConnectionStatus::Failed,
                McpClientStatus::Disconnected => McpServerConnectionStatus::NotStarted,
            },
            tools: s
                .tools
                .into_iter()
                .map(|t| {
                    let tool = McpTool {
                        name: t.name.clone(),
                        description: t.description,
                    };
                    (t.name, tool)
                })
                .collect(),
            tools_error: s.error,
            name: s.name,
        })
        .collect();
    data.sort_by(|a, b| a.name.cmp(&b.name));
    ListMcpServerStatusResponse { data }
}

/// `mcpServerStatus/list` without a session: the configured servers, none
/// started, as Codex answers from its config.
pub(crate) fn configured_status(
    configured: HashMap<String, McpServerConfig>,
    params: &ListMcpServerStatusParams,
) -> ListMcpServerStatusResponse {
    let mut data: Vec<McpServerStatus> = configured
        .into_keys()
        .filter(|name| wanted(params, name))
        .map(|name| McpServerStatus {
            name,
            runtime_status: McpServerConnectionStatus::NotStarted,
            tools: BTreeMap::new(),
            tools_error: None,
        })
        .collect();
    data.sort_by(|a, b| a.name.cmp(&b.name));
    ListMcpServerStatusResponse { data }
}

fn wanted(params: &ListMcpServerStatusParams, name: &str) -> bool {
    params
        .server_name
        .as_deref()
        .is_none_or(|only| only == name)
}

/// Send session `session_id`'s server startups as
/// `mcpServer/startupStatus/updated`.
pub(crate) fn status_fn(session_id: &str, events: &mpsc::UnboundedSender<Outgoing>) -> McpStatusFn {
    let session_id = session_id.to_string();
    let events = events.clone();
    Arc::new(move |update| {
        let (status, error) = match update.status {
            McpStartupStatus::Starting => (McpServerStartupState::Starting, None),
            McpStartupStatus::Ready => (McpServerStartupState::Ready, None),
            McpStartupStatus::Failed(why) => (McpServerStartupState::Failed, Some(why)),
            McpStartupStatus::Cancelled => (McpServerStartupState::Cancelled, None),
        };
        let _ = events.send(Outgoing::Notification(
            ServerNotification::McpServerStatusUpdated(McpServerStatusUpdatedNotification {
                session_id: session_id.clone(),
                name: update.server,
                status,
                error,
            }),
        ));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_server_round_trips_through_the_config_shape() {
        let edit = check_edit(
            "notes",
            json!({"command": "notes-mcp", "args": ["--quiet"], "env": {"A": "1"}, "required": true}),
        )
        .unwrap();
        let config = edit.unwrap();
        assert!(config.required);
        assert_eq!(config.startup_timeout_sec, DEFAULT_STARTUP_TIMEOUT_SEC);
        let value = to_value(&config);
        assert_eq!(value.command.as_deref(), Some("notes-mcp"));
        assert_eq!(value.args, ["--quiet"]);
        assert_eq!(value.startup_timeout_sec, None, "a default is left out");

        let http = check_edit(
            "mail",
            json!({"url": "https://mail.example/mcp", "bearer_token_env_var": "MAIL_TOKEN"}),
        )
        .unwrap();
        let value = to_value(&http.unwrap());
        assert_eq!(value.bearer_token_env_var.as_deref(), Some("MAIL_TOKEN"));
    }

    #[test]
    fn a_bad_server_is_refused() {
        for (name, value) in [
            ("a__b", json!({"command": "x"})),
            ("ok", json!({})),
            ("ok", json!({"command": "x", "url": "https://x"})),
            ("ok", json!({"command": "x", "bearer_token_env_var": "T"})),
            ("ok", json!({"url": "https://x", "args": ["y"]})),
            ("ok", json!({"command": "x", "bearer_token": "secret"})),
            ("ok", json!("x")),
        ] {
            assert!(check_edit(name, value.clone()).is_err(), "{name} {value}");
        }
        assert!(check_edit("ok", Value::Null).unwrap().is_none());
    }

    #[test]
    fn only_mcp_servers_dot_a_name_is_a_server_key() {
        assert_eq!(server_key("mcp_servers.notes"), Some("notes"));
        assert_eq!(server_key("mcp_servers"), None);
        assert_eq!(server_key("mcp_serversx.notes"), None);
        assert_eq!(server_key("model"), None);
    }
}
