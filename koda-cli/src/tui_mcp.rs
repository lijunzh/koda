//! MCP server management handlers for `/mcp` slash commands.
//!
//! Extracted from [`crate::tui_commands`] to keep file sizes manageable.
//! All output flows through the [`crate::scroll_buffer::ScrollBuffer`].

use crate::scroll_buffer::ScrollBuffer;
use crate::tui_output;

use koda_core::agent::KodaAgent;
use koda_core::mcp::config::{self, McpServerConfig, McpTransport, validate_server_name};
use koda_core::session::KodaSession;
use std::collections::HashMap;
use std::sync::Arc;

/// Handle `/mcp list` — show all configured servers and their status.
pub async fn handle_mcp_list(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    agent: &Arc<KodaAgent>,
) {
    // Show status from live McpManager (if attached).
    let live_statuses = get_live_statuses(agent).await;

    // Also load configs from DB for servers that may not be connected.
    let db_configs = match config::load_mcp_configs(&session.db).await {
        Ok(c) => c,
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to load MCP configs: {e}"));
            return;
        }
    };

    if db_configs.is_empty() && live_statuses.is_empty() {
        tui_output::dim_msg(
            buffer,
            "No MCP servers configured.\n\
             \n\
             Usage:\n  \
             /mcp add <name> <command> [args...]\n  \
             /mcp add-http <name> <url> [--token <bearer>]\n  \
             /mcp remove <name>\n  \
             /mcp reconnect <name>\n  \
             /mcp list\n\
             \n\
             Examples:\n  \
             /mcp add playwright npx -y @anthropic/mcp-playwright\n  \
             /mcp add-http myapi http://localhost:8080/mcp --token secret123"
                .to_string(),
        );
        return;
    }

    let mut lines: Vec<String> = vec!["MCP Servers:".to_string(), String::new()];

    // Merge DB configs with live status.
    let all_names = {
        let mut names: Vec<String> = db_configs.keys().cloned().collect();
        for status in &live_statuses {
            if !names.contains(&status.name) {
                names.push(status.name.clone());
            }
        }
        names.sort();
        names
    };

    for name in &all_names {
        let status = live_statuses.iter().find(|s| &s.name == name);
        let config = db_configs.get(name);

        let status_icon = match status {
            Some(s) => match s.status {
                koda_core::mcp::client::McpClientStatus::Connected => "🟢",
                koda_core::mcp::client::McpClientStatus::Connecting => "🟡",
                koda_core::mcp::client::McpClientStatus::Failed => "🔴",
                koda_core::mcp::client::McpClientStatus::Disconnected => "⚪",
            },
            None => "⚪",
        };

        let tools_info = match status {
            Some(s) if s.tool_count > 0 => format!(" ({} tools)", s.tool_count),
            _ => String::new(),
        };

        let cmd_info = match config {
            Some(c) => match &c.transport {
                McpTransport::Stdio { command, args, .. } => {
                    let full = if args.is_empty() {
                        command.clone()
                    } else {
                        format!("{command} {}", args.join(" "))
                    };
                    format!("  cmd: {full}")
                }
                McpTransport::Http { url, .. } => {
                    format!("  url: {url}")
                }
            },
            None => String::new(),
        };

        let error_info = match status {
            Some(s) if s.error.is_some() => {
                format!("  error: {}", s.error.as_deref().unwrap_or(""))
            }
            _ => String::new(),
        };

        lines.push(format!("  {status_icon} {name}{tools_info}"));
        if !cmd_info.is_empty() {
            lines.push(cmd_info);
        }
        if !error_info.is_empty() {
            lines.push(error_info);
        }
    }

    tui_output::dim_msg(buffer, lines.join("\n"));
}

/// Handle `/mcp add <name> <command> [args...]`.
pub async fn handle_mcp_add(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    agent: &Arc<KodaAgent>,
    name: String,
    command: String,
    args: Vec<String>,
) {
    let config = McpServerConfig {
        transport: McpTransport::Stdio {
            command: command.clone(),
            args: args.clone(),
            env: HashMap::new(),
            cwd: None,
        },
        startup_timeout_sec: 30,
        tool_timeout_sec: 120,
        enabled_tools: None,
        disabled_tools: None,
    };

    // Validate name, then config.
    if let Err(e) = validate_server_name(&name) {
        tui_output::err_msg(buffer, format!("Invalid server name: {e}"));
        return;
    }
    // Validate first.
    if let Err(e) = config.validate() {
        tui_output::err_msg(buffer, format!("Invalid config: {e}"));
        return;
    }

    // Save to DB.
    if let Err(e) = config::save_mcp_config(&session.db, &name, &config).await {
        tui_output::err_msg(buffer, format!("Failed to save config: {e}"));
        return;
    }

    // Hot-reload: connect immediately if we have a manager.
    let connect_result = try_add_to_live_manager(agent, name.clone(), config).await;

    match connect_result {
        Some(Ok((tool_count,))) => {
            let cmd_display = if args.is_empty() {
                command
            } else {
                format!("{command} {}", args.join(" "))
            };
            tui_output::ok_msg(
                buffer,
                format!(
                    "MCP server '{name}' added and connected ({tool_count} tools)\n  cmd: {cmd_display}"
                ),
            );
        }
        Some(Err(e)) => {
            tui_output::warn_msg(
                buffer,
                format!(
                    "MCP server '{name}' saved but failed to connect: {e}\n\
                     It will retry on next session start."
                ),
            );
        }
        None => {
            tui_output::ok_msg(
                buffer,
                format!("MCP server '{name}' saved. Will connect on next session start."),
            );
        }
    }
}

/// Handle `/mcp add-http <name> <url> [--token <bearer>]`.
pub async fn handle_mcp_add_http(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    agent: &Arc<KodaAgent>,
    name: String,
    url: String,
    bearer_token: Option<String>,
) {
    let config = McpServerConfig {
        transport: McpTransport::Http {
            url: url.clone(),
            bearer_token: bearer_token.clone(),
            headers: HashMap::new(),
        },
        startup_timeout_sec: 30,
        tool_timeout_sec: 120,
        enabled_tools: None,
        disabled_tools: None,
    };

    // Validate name, then config.
    if let Err(e) = validate_server_name(&name) {
        tui_output::err_msg(buffer, format!("Invalid server name: {e}"));
        return;
    }
    if let Err(e) = config.validate() {
        tui_output::err_msg(buffer, format!("Invalid config: {e}"));
        return;
    }

    // Persist to DB.
    if let Err(e) = config::save_mcp_config(&session.db, &name, &config).await {
        tui_output::err_msg(buffer, format!("Failed to save MCP config: {e}"));
        return;
    }

    // Hot-reload: connect immediately if we have a manager.
    let connect_result = try_add_to_live_manager(agent, name.clone(), config).await;

    let token_hint = if bearer_token.is_some() {
        " (with auth)"
    } else {
        ""
    };

    match connect_result {
        Some(Ok((tool_count,))) => {
            tui_output::ok_msg(
                buffer,
                format!(
                    "MCP server '{name}' added via HTTP and connected ({tool_count} tools)\n  url: {url}{token_hint}"
                ),
            );
        }
        Some(Err(e)) => {
            tui_output::warn_msg(
                buffer,
                format!(
                    "MCP server '{name}' saved but failed to connect: {e}\n\
                     It will retry on next session start."
                ),
            );
        }
        None => {
            tui_output::ok_msg(
                buffer,
                format!("MCP server '{name}' saved. Will connect on next session start."),
            );
        }
    }
}

/// Handle `/mcp remove <name>`.
pub async fn handle_mcp_remove(
    buffer: &mut ScrollBuffer,
    session: &KodaSession,
    agent: &Arc<KodaAgent>,
    name: String,
) {
    // Check it exists in DB.
    let exists = match config::list_mcp_server_names(&session.db).await {
        Ok(names) => names.contains(&name),
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to check MCP configs: {e}"));
            return;
        }
    };

    if !exists {
        tui_output::err_msg(buffer, format!("MCP server '{name}' not found."));
        return;
    }

    // Remove from DB.
    if let Err(e) = config::remove_mcp_config(&session.db, &name).await {
        tui_output::err_msg(buffer, format!("Failed to remove config: {e}"));
        return;
    }

    // Hot-reload: disconnect if live.
    let was_live = try_remove_from_live_manager(agent, &name).await;

    let extra = if was_live { " and disconnected" } else { "" };
    tui_output::ok_msg(buffer, format!("MCP server '{name}' removed{extra}."));
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Get live status summaries from the McpManager (if attached).
async fn get_live_statuses(
    agent: &Arc<KodaAgent>,
) -> Vec<koda_core::mcp::manager::McpServerStatus> {
    let Some(mgr) = agent.tools.mcp_manager() else {
        return vec![];
    };
    let Ok(mgr) = mgr.try_read() else {
        return vec![];
    };
    mgr.status_summary()
}

/// Try to add a server to the live McpManager. Returns None if no manager.
async fn try_add_to_live_manager(
    agent: &Arc<KodaAgent>,
    name: String,
    config: McpServerConfig,
) -> Option<Result<(usize,), anyhow::Error>> {
    let mgr = agent.tools.mcp_manager()?;
    let mut mgr = mgr.write().await;
    let result = mgr.add_server(name.clone(), config).await;
    match &result {
        Ok(()) => {
            let tool_count = mgr
                .status_summary()
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.tool_count)
                .unwrap_or(0);
            Some(Ok((tool_count,)))
        }
        Err(e) => Some(Err(anyhow::anyhow!("{e}"))),
    }
}

/// Try to remove a server from the live McpManager. Returns whether it was live.
async fn try_remove_from_live_manager(agent: &Arc<KodaAgent>, name: &str) -> bool {
    let Some(mgr) = agent.tools.mcp_manager() else {
        return false;
    };
    let mut mgr = mgr.write().await;
    mgr.remove_server(name).await
}

/// Handle `/mcp reconnect <name>`.
pub async fn handle_mcp_reconnect(buffer: &mut ScrollBuffer, agent: &Arc<KodaAgent>, name: String) {
    let Some(mgr) = agent.tools.mcp_manager() else {
        tui_output::err_msg(buffer, "No MCP manager available.".into());
        return;
    };

    tui_output::dim_msg(buffer, format!("Reconnecting MCP server '{name}'..."));

    let result = {
        let mut mgr = mgr.write().await;
        mgr.reconnect_server(&name).await
    };

    match result {
        Ok(tool_count) => {
            tui_output::ok_msg(
                buffer,
                format!("MCP server '{name}' reconnected ({tool_count} tools)"),
            );
        }
        Err(e) => {
            tui_output::err_msg(buffer, format!("Failed to reconnect '{name}': {e}"));
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scroll_buffer::ScrollBuffer;
    use koda_core::config::{KodaConfig, ProviderType};
    use koda_core::db::Database;
    use koda_core::session::KodaSession;
    use koda_core::trust::TrustMode;
    use tempfile::TempDir;

    // ── Helpers ───────────────────────────────────────────────────

    fn make_buffer() -> ScrollBuffer {
        ScrollBuffer::new(100)
    }

    /// Flatten all span content in the buffer into a single string for assertions.
    fn buffer_text(buf: &ScrollBuffer) -> String {
        buf.all_lines()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    /// Build a minimal test agent + session (no API key needed — mock provider).
    async fn make_agent_and_session(tmp: &TempDir) -> (Arc<KodaAgent>, KodaSession) {
        let mut config = KodaConfig::load(tmp.path(), "default").unwrap();
        // Swap to mock so KodaSession::new() doesn’t warn about missing API key.
        config.provider_type = ProviderType::Mock;

        let agent = Arc::new(
            KodaAgent::new(&config, tmp.path().to_owned(), &[])
                .await
                .unwrap(),
        );
        let db = Database::open(&tmp.path().join("koda.db")).await.unwrap();
        let session = KodaSession::new(
            "test-session".into(),
            Arc::clone(&agent),
            db,
            &config,
            TrustMode::Safe,
        )
        .await;
        (agent, session)
    }

    // ── handle_mcp_reconnect: no manager ──────────────────────────────

    #[tokio::test]
    async fn reconnect_with_no_manager_shows_error() {
        let tmp = TempDir::new().unwrap();
        let (agent, _session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        // Fresh session with empty DB → no MCP servers → no manager attached.
        handle_mcp_reconnect(&mut buf, &agent, "myserver".into()).await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("No MCP manager"),
            "expected 'No MCP manager' in: {text}"
        );
    }

    // ── handle_mcp_list: empty DB ───────────────────────────────────

    #[tokio::test]
    async fn list_empty_db_shows_usage_hint() {
        let tmp = TempDir::new().unwrap();
        let (agent, session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        handle_mcp_list(&mut buf, &session, &agent).await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("/mcp add"),
            "expected usage hint with '/mcp add' in: {text}"
        );
        assert!(
            text.contains("No MCP servers configured"),
            "expected 'No MCP servers configured' in: {text}"
        );
    }

    // ── handle_mcp_add: validation errors ────────────────────────────

    #[tokio::test]
    async fn add_with_invalid_name_shows_validation_error() {
        let tmp = TempDir::new().unwrap();
        let (agent, session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        // Names with spaces are invalid.
        handle_mcp_add(
            &mut buf,
            &session,
            &agent,
            "bad name".into(),
            "npx".into(),
            vec![],
        )
        .await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("Invalid server name"),
            "expected validation error in: {text}"
        );
    }

    #[tokio::test]
    async fn add_with_empty_command_shows_config_error() {
        let tmp = TempDir::new().unwrap();
        let (agent, session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        // Empty command string → config validate() should reject it.
        handle_mcp_add(
            &mut buf,
            &session,
            &agent,
            "valid-name".into(),
            String::new(),
            vec![],
        )
        .await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("Invalid config") || text.contains("empty"),
            "expected config validation error in: {text}"
        );
    }

    // ── handle_mcp_add_http: token hint ───────────────────────────────

    #[tokio::test]
    async fn add_http_with_invalid_name_shows_validation_error() {
        let tmp = TempDir::new().unwrap();
        let (agent, session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        handle_mcp_add_http(
            &mut buf,
            &session,
            &agent,
            "bad name!".into(),
            "http://localhost:8080/mcp".into(),
            None,
        )
        .await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("Invalid server name"),
            "expected name validation error in: {text}"
        );
    }

    // ── handle_mcp_remove: nonexistent server ─────────────────────────

    #[tokio::test]
    async fn remove_nonexistent_server_shows_error() {
        let tmp = TempDir::new().unwrap();
        let (agent, session) = make_agent_and_session(&tmp).await;
        let mut buf = make_buffer();

        handle_mcp_remove(&mut buf, &session, &agent, "ghost-server".into()).await;

        let text = buffer_text(&buf);
        assert!(
            text.contains("ghost-server"),
            "error should mention the server name in: {text}"
        );
        // Should show an error (not found / remove failed).
        assert!(
            text.contains("not found") || text.contains("Failed") || text.contains("no server"),
            "expected not-found or error message in: {text}"
        );
    }
}
