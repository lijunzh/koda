//! MCP server configuration types and database persistence.
//!
//! Configs are stored in the SQLite `kv_store` table under `mcp:<server_name>`
//! keys as JSON blobs. This is consistent with how Koda stores settings
//! (`setting:*`) and API keys (`apikey:*`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::db::Database;

/// Key prefix for MCP server configs in the kv_store.
const MCP_KEY_PREFIX: &str = "mcp:";

/// Default timeout (seconds) for MCP server startup.
const DEFAULT_STARTUP_TIMEOUT_SEC: u64 = 30;

/// Default timeout (seconds) for individual tool calls.
const DEFAULT_TOOL_TIMEOUT_SEC: u64 = 120;

/// Transport type for connecting to an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    /// Spawn a child process and communicate over stdin/stdout.
    Stdio {
        /// Command to spawn.
        command: String,
        /// Arguments passed to the command.
        #[serde(default)]
        args: Vec<String>,
        /// Additional environment variables for the server process.
        #[serde(default)]
        env: HashMap<String, String>,
        /// Working directory for the server process.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Connect via Streamable HTTP (MCP 2025-03-26 spec).
    Http {
        /// Server URL (e.g. `http://localhost:8080/mcp`).
        url: String,
        /// Optional bearer token for `Authorization: Bearer <token>`.
        #[serde(default)]
        bearer_token: Option<String>,
        /// Custom HTTP headers included with every request.
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Configuration for a single MCP server.
///
/// Supports stdio and HTTP (Streamable HTTP) transports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// Transport configuration.
    #[serde(flatten)]
    pub transport: McpTransport,

    // ── Timeouts ───────────────────────────────────────────────────────
    /// Seconds to wait for the server to start and respond to `initialize`.
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_sec: u64,

    /// Seconds to wait for a single tool call to complete.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_sec: u64,

    // ── Tool filtering ────────────────────────────────────────────────
    /// If set, only expose these tools (allowlist). Others are hidden.
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,

    /// If set, hide these tools (denylist). `enabled_tools` takes priority.
    #[serde(default)]
    pub disabled_tools: Option<Vec<String>>,
}

fn default_startup_timeout() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SEC
}

fn default_tool_timeout() -> u64 {
    DEFAULT_TOOL_TIMEOUT_SEC
}

impl McpServerConfig {
    /// Validate the config. Returns an error if essential fields are missing.
    pub fn validate(&self) -> Result<()> {
        match &self.transport {
            McpTransport::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    anyhow::bail!("MCP server config must specify a `command`");
                }
            }
            McpTransport::Http { url, .. } => {
                if url.trim().is_empty() {
                    anyhow::bail!("MCP server config must specify a `url`");
                }
            }
        }
        Ok(())
    }

    /// Check whether a tool name passes the include/exclude filter.
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        if let Some(ref enabled) = self.enabled_tools {
            return enabled.iter().any(|t| t == tool_name);
        }
        if let Some(ref disabled) = self.disabled_tools {
            return !disabled.iter().any(|t| t == tool_name);
        }
        true
    }
}

// ── Database persistence ──────────────────────────────────────────────────

/// Load all MCP server configs from the database.
pub async fn load_mcp_configs(db: &Database) -> Result<HashMap<String, McpServerConfig>> {
    let rows = db
        .kv_list_prefix(MCP_KEY_PREFIX)
        .await
        .context("failed to load MCP configs from kv_store")?;

    let mut configs = HashMap::new();
    for (key, value) in rows {
        let server_name = key.strip_prefix(MCP_KEY_PREFIX).unwrap_or(&key).to_string();
        if server_name.is_empty() {
            continue;
        }
        match serde_json::from_str::<McpServerConfig>(&value) {
            Ok(config) => {
                configs.insert(server_name, config);
            }
            Err(e) => {
                tracing::warn!(
                    server = %server_name,
                    error = %e,
                    "skipping MCP server with invalid config"
                );
            }
        }
    }
    Ok(configs)
}

/// Save an MCP server config to the database.
pub async fn save_mcp_config(db: &Database, name: &str, config: &McpServerConfig) -> Result<()> {
    config.validate()?;
    let key = format!("{MCP_KEY_PREFIX}{name}");
    let value = serde_json::to_string(config).context("failed to serialize MCP config")?;
    db.kv_set(&key, &value)
        .await
        .context("failed to save MCP config to kv_store")
}

/// Remove an MCP server config from the database.
pub async fn remove_mcp_config(db: &Database, name: &str) -> Result<()> {
    let key = format!("{MCP_KEY_PREFIX}{name}");
    db.kv_delete(&key)
        .await
        .context("failed to remove MCP config from kv_store")
}

/// List all configured MCP server names.
pub async fn list_mcp_server_names(db: &Database) -> Result<Vec<String>> {
    let rows = db
        .kv_list_prefix(MCP_KEY_PREFIX)
        .await
        .context("failed to list MCP servers from kv_store")?;

    Ok(rows
        .into_iter()
        .filter_map(|(key, _)| {
            key.strip_prefix(MCP_KEY_PREFIX)
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a stdio config with defaults.
    fn stdio_config(command: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            startup_timeout_sec: 30,
            tool_timeout_sec: 120,
            enabled_tools: None,
            disabled_tools: None,
        }
    }

    /// Helper: build an HTTP config with defaults.
    fn http_config(url: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Http {
                url: url.into(),
                bearer_token: None,
                headers: HashMap::new(),
            },
            startup_timeout_sec: 30,
            tool_timeout_sec: 120,
            enabled_tools: None,
            disabled_tools: None,
        }
    }

    #[test]
    fn validate_rejects_empty_command() {
        assert!(stdio_config("").validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_url() {
        assert!(http_config("").validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_stdio_config() {
        assert!(stdio_config("npx").validate().is_ok());
    }

    #[test]
    fn validate_accepts_valid_http_config() {
        assert!(http_config("http://localhost:8080/mcp").validate().is_ok());
    }

    #[test]
    fn tool_filter_allowlist() {
        let mut config = stdio_config("test");
        config.enabled_tools = Some(vec!["navigate".into(), "click".into()]);
        assert!(config.is_tool_allowed("navigate"));
        assert!(config.is_tool_allowed("click"));
        assert!(!config.is_tool_allowed("screenshot"));
    }

    #[test]
    fn tool_filter_denylist() {
        let mut config = stdio_config("test");
        config.disabled_tools = Some(vec!["dangerous_tool".into()]);
        assert!(config.is_tool_allowed("navigate"));
        assert!(!config.is_tool_allowed("dangerous_tool"));
    }

    #[test]
    fn tool_filter_allowlist_beats_denylist() {
        let mut config = stdio_config("test");
        config.enabled_tools = Some(vec!["safe".into()]);
        config.disabled_tools = Some(vec!["safe".into()]); // contradicts — allowlist wins
        assert!(config.is_tool_allowed("safe"));
        assert!(!config.is_tool_allowed("other"));
    }

    #[test]
    fn roundtrip_serde_stdio() {
        let config = McpServerConfig {
            transport: McpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "playwright-mcp".into()],
                env: HashMap::from([("FOO".into(), "bar".into())]),
                cwd: Some("/tmp".into()),
            },
            startup_timeout_sec: 10,
            tool_timeout_sec: 60,
            enabled_tools: Some(vec!["navigate".into()]),
            disabled_tools: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn roundtrip_serde_http() {
        let config = McpServerConfig {
            transport: McpTransport::Http {
                url: "http://localhost:8080/mcp".into(),
                bearer_token: Some("my-secret".into()),
                headers: HashMap::from([("X-Custom".into(), "value".into())]),
            },
            startup_timeout_sec: 15,
            tool_timeout_sec: 90,
            enabled_tools: None,
            disabled_tools: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn serde_defaults_applied_stdio() {
        let json = r#"{"transport": "stdio", "command": "npx", "args": ["-y", "test"]}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.startup_timeout_sec, 30);
        assert_eq!(config.tool_timeout_sec, 120);
        assert!(config.enabled_tools.is_none());
    }

    #[test]
    fn serde_defaults_applied_http() {
        let json = r#"{"transport": "http", "url": "http://localhost:8080/mcp"}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.startup_timeout_sec, 30);
        assert!(matches!(config.transport, McpTransport::Http { .. }));
    }
}
