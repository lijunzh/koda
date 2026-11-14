//! `/mcp` and the status bar's MCP pill, through the host: Codex's TUI
//! lists servers with `mcpServerStatus/list` (`/mcp`, `/mcp verbose`) and
//! follows `mcpServer/startupStatus/updated`. Servers are added and removed
//! from a terminal with `koda mcp` (`crate::mcp_cmd`), as with `codex mcp`;
//! `/mcp reconnect` has open sessions take the change.

use std::collections::BTreeMap;

use koda_client::events::Outgoing;
use koda_client::protocol::ServerNotification;
use koda_client::protocol::config::{ConfigLayerSource, ConfigReadParams, ConfigReadResponse};
use koda_client::protocol::mcp::{
    ListMcpServerStatusParams, McpServerConfigValue, McpServerConnectionStatus,
    McpServerStartupState, McpServerStatus,
};
use koda_client::protocol::methods;

use super::TuiContext;
use crate::tui_output;
use crate::widgets::status_bar::McpStatusBarInfo;

/// Where each of the session's servers stands, for the status bar.
#[derive(Debug, Default)]
pub(crate) struct McpView {
    servers: BTreeMap<String, McpServerConnectionStatus>,
}

impl McpView {
    fn replace(&mut self, listed: &[McpServerStatus]) {
        self.servers = listed
            .iter()
            .map(|s| (s.name.clone(), s.runtime_status))
            .collect();
    }

    /// Follow a startup report for session `session_id`.
    pub(crate) fn observe(&mut self, session_id: &str, message: &Outgoing) {
        let Outgoing::Notification(ServerNotification::McpServerStatusUpdated(update)) = message
        else {
            return;
        };
        if update.session_id != session_id {
            return;
        }
        let status = match update.status {
            McpServerStartupState::Starting => McpServerConnectionStatus::Starting,
            McpServerStartupState::Ready => McpServerConnectionStatus::Connected,
            McpServerStartupState::Failed => McpServerConnectionStatus::Failed,
            McpServerStartupState::Cancelled => {
                self.servers.remove(&update.name);
                return;
            }
        };
        self.servers.insert(update.name.clone(), status);
    }

    /// The pill: hidden with no servers.
    pub(crate) fn status_bar(&self) -> Option<McpStatusBarInfo> {
        if self.servers.is_empty() {
            return None;
        }
        let count = |wanted| self.servers.values().filter(|s| **s == wanted).count();
        Some(McpStatusBarInfo {
            connected: count(McpServerConnectionStatus::Connected),
            failed: count(McpServerConnectionStatus::Failed),
            total: self.servers.len(),
        })
    }
}

const MANAGE_HINT: &str = "Add and remove servers from a terminal, then `/mcp reconnect` here:\n  \
     koda mcp add <name> -- <command> [args...]\n  \
     koda mcp add <name> --url <url> [--bearer-token-env-var <VAR>]\n  \
     koda mcp remove <name>\n  \
     koda mcp list";

impl TuiContext {
    /// The session's servers, fresh from the host, for the status bar.
    pub(crate) async fn refresh_mcp(&mut self) -> Option<Vec<McpServerStatus>> {
        let listed = self
            .client
            .request::<methods::McpServerStatusList>(ListMcpServerStatusParams {
                session_id: Some(self.session.id.clone()),
                server_name: None,
            })
            .await;
        match listed {
            Ok(listed) => {
                self.mcp.replace(&listed.data);
                self.mcp_stale = false;
                Some(listed.data)
            }
            Err(e) => {
                tracing::warn!("couldn't list MCP servers: {e}");
                None
            }
        }
    }

    /// `/mcp [verbose | reconnect]`.
    pub(crate) async fn handle_mcp(&mut self, arg: &str) {
        let mut words = arg.split_whitespace();
        match words.next().unwrap_or("") {
            "" | "list" | "status" => self.show_mcp(false).await,
            "verbose" => self.show_mcp(true).await,
            "reconnect" | "reload" | "retry" | "restart" => {
                let reloaded = self
                    .client
                    .request::<methods::McpServerRefresh>(Default::default())
                    .await;
                match reloaded {
                    Ok(_) => {
                        tui_output::ok_msg(
                            &mut self.scroll_buffer,
                            "Reloaded MCP servers: changed and failed ones start again.".into(),
                        );
                        self.show_mcp(false).await;
                    }
                    Err(e) => tui_output::err_msg(
                        &mut self.scroll_buffer,
                        format!("Couldn't reload MCP servers: {e}"),
                    ),
                }
            }
            "add" | "add-http" | "add_http" | "remove" | "rm" | "delete" => {
                tui_output::warn_msg(&mut self.scroll_buffer, MANAGE_HINT.to_string());
            }
            other => tui_output::warn_msg(
                &mut self.scroll_buffer,
                format!(
                    "Unknown /mcp command `{other}`. Try /mcp, /mcp verbose or /mcp reconnect."
                ),
            ),
        }
    }

    /// List the session's servers; `verbose` adds each one's config and
    /// what its tools do.
    async fn show_mcp(&mut self, verbose: bool) {
        let Some(listed) = self.refresh_mcp().await else {
            tui_output::err_msg(&mut self.scroll_buffer, "Couldn't list MCP servers.".into());
            return;
        };
        if listed.is_empty() {
            tui_output::dim_msg(
                &mut self.scroll_buffer,
                format!("No MCP servers configured.\n\n{MANAGE_HINT}"),
            );
            return;
        }
        let read = if verbose {
            self.client
                .request::<methods::ConfigRead>(ConfigReadParams::default())
                .await
                .ok()
        } else {
            None
        };
        let configs = read
            .as_ref()
            .map(|read| read.config.mcp_servers.clone())
            .unwrap_or_default();
        let mut lines = vec!["MCP Servers:".to_string(), String::new()];
        for server in &listed {
            let mut server_lines = server_lines(server, configs.get(&server.name), verbose);
            if let Some(read) = &read {
                server_lines.insert(1, format!("     from {}", origin(read, &server.name)));
            }
            lines.extend(server_lines);
        }
        tui_output::dim_msg(&mut self.scroll_buffer, lines.join("\n"));
    }
}

/// Where server `name` comes from: the config file, or this run's
/// `--mcp-config`, which `config/read` doesn't list.
fn origin(read: &ConfigReadResponse, name: &str) -> String {
    let prefix = format!("mcp_servers.{name}.");
    read.origins
        .iter()
        .find(|(key, _)| key.starts_with(&prefix))
        .map(|(_, origin)| match &origin.name {
            ConfigLayerSource::User { file } => file.clone(),
            ConfigLayerSource::SessionFlags => "-c (this run only)".to_string(),
        })
        .unwrap_or_else(|| "--mcp-config (this run only)".to_string())
}

fn server_lines(
    server: &McpServerStatus,
    config: Option<&McpServerConfigValue>,
    verbose: bool,
) -> Vec<String> {
    let (icon, state) = match server.runtime_status {
        McpServerConnectionStatus::Connected => ("\u{1f7e2}", String::new()),
        McpServerConnectionStatus::Starting => ("\u{1f7e1}", " starting".to_string()),
        McpServerConnectionStatus::Failed => ("\u{1f534}", " failed".to_string()),
        McpServerConnectionStatus::NotStarted => ("\u{26aa}", " not started".to_string()),
    };
    let mut lines = vec![format!(
        "  {icon} {} ({} tools){state}",
        server.name,
        server.tools.len()
    )];
    if let Some(error) = &server.tools_error {
        lines.push(format!("     error: {error}"));
    }
    if verbose {
        if let Some(config) = config {
            if let Some(command) = &config.command {
                let mut cmd = vec![command.clone()];
                cmd.extend(config.args.iter().cloned());
                lines.push(format!("     cmd: {}", cmd.join(" ")));
            }
            if let Some(url) = &config.url {
                lines.push(format!("     url: {url}"));
            }
            if let Some(var) = &config.bearer_token_env_var {
                lines.push(format!("     token: ${var}"));
            }
            if config.required {
                lines.push("     required".to_string());
            }
        }
        for tool in server.tools.values() {
            match &tool.description {
                Some(description) => lines.push(format!("     {}: {description}", tool.name)),
                None => lines.push(format!("     {}", tool.name)),
            }
        }
    } else if !server.tools.is_empty() {
        let names: Vec<&str> = server.tools.keys().map(String::as_str).collect();
        lines.push(format!("     {}", names.join(", ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_client::protocol::mcp::McpServerStatusUpdatedNotification;

    fn update(session: &str, name: &str, status: McpServerStartupState) -> Outgoing {
        Outgoing::Notification(ServerNotification::McpServerStatusUpdated(
            McpServerStatusUpdatedNotification {
                session_id: session.into(),
                name: name.into(),
                status,
                error: None,
            },
        ))
    }

    #[test]
    fn the_pill_follows_this_sessions_startups() {
        let mut view = McpView::default();
        assert_eq!(view.status_bar(), None);
        view.observe("s", &update("s", "a", McpServerStartupState::Starting));
        view.observe("s", &update("s", "b", McpServerStartupState::Failed));
        view.observe("s", &update("other", "c", McpServerStartupState::Ready));
        assert_eq!(
            view.status_bar(),
            Some(McpStatusBarInfo {
                connected: 0,
                failed: 1,
                total: 2
            })
        );
        view.observe("s", &update("s", "a", McpServerStartupState::Ready));
        view.observe("s", &update("s", "b", McpServerStartupState::Cancelled));
        assert_eq!(
            view.status_bar(),
            Some(McpStatusBarInfo {
                connected: 1,
                failed: 0,
                total: 1
            })
        );
    }
}
