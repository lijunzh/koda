//! MCP server configuration types and database persistence.
//!
//! Configs are stored in the SQLite `kv_store` table under `mcp:<server_name>`
//! keys as JSON blobs. This is consistent with how Koda stores settings
//! (`setting:*`) and API keys (`apikey:*`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use crate::db::Database;

/// Key prefix for MCP server configs in the kv_store.
const MCP_KEY_PREFIX: &str = "mcp:";

/// Default timeout (seconds) for MCP server startup.
const DEFAULT_STARTUP_TIMEOUT_SEC: u64 = 30;

/// Default timeout (seconds) for individual tool calls.
const DEFAULT_TOOL_TIMEOUT_SEC: u64 = 120;

/// Transport type for connecting to an MCP server.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
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

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", &env.keys().collect::<Vec<_>>())
                .field("cwd", cwd)
                .finish(),
            McpTransport::Http {
                url,
                bearer_token,
                headers,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field("bearer_token", &bearer_token.as_ref().map(|_| "[redacted]"))
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
        }
    }
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

/// Validate an MCP server name.
///
/// Names must be non-empty, contain only `[a-zA-Z0-9_-]`, and must not
/// contain `__` (double-underscore), which is the tool-name separator used
/// internally by Koda's tool routing (`<server>__<tool>`).
pub fn validate_server_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        anyhow::bail!("MCP server name must not be empty");
    }
    if name.contains("__") {
        anyhow::bail!(
            "MCP server name '{name}' must not contain `__` \
             (double-underscore is the tool-routing separator)"
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "MCP server name '{name}' must contain only letters, digits, hyphens, \
             or underscores"
        );
    }
    Ok(())
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

// ── Per-run servers (`--mcp-config`) ──────────────────────────────────────

/// One server entry in the `mcpServers` JSON shape that Claude Code, Qwen
/// Code and Gemini CLI share. Fields other agents add are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonEntry {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(alias = "startup_timeout_sec")]
    startup_timeout_sec: Option<u64>,
    #[serde(alias = "tool_timeout_sec")]
    tool_timeout_sec: Option<u64>,
    #[serde(alias = "enabled_tools")]
    enabled_tools: Option<Vec<String>>,
    #[serde(alias = "disabled_tools")]
    disabled_tools: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonFile {
    mcp_servers: HashMap<String, McpJsonEntry>,
}

impl McpJsonEntry {
    fn into_config(self, name: &str) -> Result<McpServerConfig> {
        let kind = self.kind.as_deref().map(str::to_ascii_lowercase);
        let transport = match (kind.as_deref(), self.command, self.url) {
            (None | Some("stdio"), Some(command), None) => McpTransport::Stdio {
                command,
                args: self.args,
                env: self.env,
                cwd: self.cwd,
            },
            (
                None | Some("http") | Some("streamable-http") | Some("streamable_http"),
                None,
                Some(url),
            ) => McpTransport::Http {
                url,
                bearer_token: None,
                headers: self.headers,
            },
            (Some("sse"), ..) => {
                anyhow::bail!(
                    "MCP server '{name}': the SSE transport isn't supported; use stdio or http"
                )
            }
            (Some(other), ..)
                if !["stdio", "http", "streamable-http", "streamable_http"].contains(&other) =>
            {
                anyhow::bail!("MCP server '{name}': unknown type '{other}'; use stdio or http")
            }
            (_, Some(_), Some(_)) => {
                anyhow::bail!("MCP server '{name}': give either `command` or `url`, not both")
            }
            _ => anyhow::bail!(
                "MCP server '{name}': needs `command` (stdio) or `url` (http) to match its type"
            ),
        };
        let config = McpServerConfig {
            transport,
            startup_timeout_sec: self
                .startup_timeout_sec
                .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SEC),
            tool_timeout_sec: self.tool_timeout_sec.unwrap_or(DEFAULT_TOOL_TIMEOUT_SEC),
            enabled_tools: self.enabled_tools,
            disabled_tools: self.disabled_tools,
        };
        config
            .validate()
            .with_context(|| format!("MCP server '{name}'"))?;
        Ok(config)
    }
}

/// Parse one `--mcp-config` value: a path to a JSON file, or the JSON itself
/// when the value starts with `{`. The shape is `{"mcpServers": {...}}`, as
/// in Claude Code's `.mcp.json`.
///
/// These servers last for one run: they are never written to the database.
/// Any error is returned, not logged and skipped, because a flag the caller
/// typed that silently does nothing is worse than a refusal to start.
pub fn parse_mcp_config_arg(arg: &str) -> Result<HashMap<String, McpServerConfig>> {
    let (text, source) = if arg.trim_start().starts_with('{') {
        (arg.to_string(), "inline JSON".to_string())
    } else {
        let text = std::fs::read_to_string(arg)
            .with_context(|| format!("--mcp-config: can't read {arg}"))?;
        (text, arg.to_string())
    };
    let file: McpJsonFile = serde_json::from_str(&text).with_context(|| {
        format!("--mcp-config: {source} isn't {{\"mcpServers\": {{...}}}} JSON")
    })?;
    let mut servers = HashMap::new();
    for (name, entry) in file.mcp_servers {
        validate_server_name(&name).context("--mcp-config")?;
        let config = entry
            .into_config(&name)
            .with_context(|| format!("--mcp-config: {source}"))?;
        servers.insert(name, config);
    }
    Ok(servers)
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

    // ── validate_server_name ──────────────────────────────────────────────

    #[test]
    fn server_name_rejects_empty() {
        assert!(validate_server_name("").is_err());
        assert!(validate_server_name("   ").is_err());
    }

    #[test]
    fn server_name_rejects_double_underscore() {
        assert!(validate_server_name("my__server").is_err());
        assert!(validate_server_name("__leading").is_err());
        assert!(validate_server_name("trailing__").is_err());
    }

    #[test]
    fn server_name_rejects_invalid_chars() {
        assert!(validate_server_name("my server").is_err()); // space
        assert!(validate_server_name("my.server").is_err()); // dot
        assert!(validate_server_name("my/server").is_err()); // slash
    }

    #[test]
    fn server_name_accepts_valid() {
        assert!(validate_server_name("playwright").is_ok());
        assert!(validate_server_name("my-server").is_ok());
        assert!(validate_server_name("server_1").is_ok()); // single underscore fine
        assert!(validate_server_name("MyServer123").is_ok());
    }

    // ── McpTransport Debug redaction ─────────────────────────────────────

    #[test]
    fn debug_redacts_bearer_token() {
        let t = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            bearer_token: Some("super-secret-token".into()),
            headers: HashMap::new(),
        };
        let debug_output = format!("{t:?}");
        assert!(
            !debug_output.contains("super-secret-token"),
            "bearer token must not appear in Debug output: {debug_output}"
        );
        assert!(
            debug_output.contains("[redacted]"),
            "expected '[redacted]' in Debug output: {debug_output}"
        );
    }

    #[test]
    fn debug_shows_none_when_no_token() {
        let t = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            bearer_token: None,
            headers: HashMap::new(),
        };
        let debug_output = format!("{t:?}");
        assert!(
            !debug_output.contains("[redacted]"),
            "no token → should not show [redacted]: {debug_output}"
        );
    }

    // ── --mcp-config ─────────────────────────────────────────────────────

    #[test]
    fn mcp_config_arg_reads_claude_code_shape_inline() {
        let servers = parse_mcp_config_arg(
            r#"{"mcpServers": {
                "mail": {"command": "mail-mcp", "args": ["--ro"], "env": {"K": "v"}, "startupTimeoutSec": 5},
                "api": {"type": "http", "url": "https://api.example.com/mcp", "headers": {"X-Key": "k"}},
                "bare": {"url": "https://other.example.com/mcp"}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 3);
        let mail = &servers["mail"];
        assert_eq!(mail.startup_timeout_sec, 5);
        assert_eq!(mail.tool_timeout_sec, DEFAULT_TOOL_TIMEOUT_SEC);
        match &mail.transport {
            McpTransport::Stdio {
                command, args, env, ..
            } => {
                assert_eq!(command, "mail-mcp");
                assert_eq!(args, &["--ro"]);
                assert_eq!(env["K"], "v");
            }
            other => panic!("expected stdio, got {other:?}"),
        }
        assert!(
            matches!(&servers["api"].transport, McpTransport::Http { headers, .. } if headers["X-Key"] == "k")
        );
        assert!(matches!(
            &servers["bare"].transport,
            McpTransport::Http { .. }
        ));
    }

    #[test]
    fn mcp_config_arg_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers": {"x": {"command": "x-mcp"}}}"#).unwrap();
        let servers = parse_mcp_config_arg(path.to_str().unwrap()).unwrap();
        assert!(servers.contains_key("x"));
    }

    #[test]
    fn mcp_config_arg_refuses_what_it_cannot_run() {
        for (arg, want) in [
            (
                r#"{"mcpServers": {"s": {"type": "sse", "url": "https://e.com/sse"}}}"#,
                "SSE",
            ),
            (
                r#"{"mcpServers": {"s": {"command": "a", "url": "https://e.com"}}}"#,
                "not both",
            ),
            (
                r#"{"mcpServers": {"s": {"args": ["a"]}}}"#,
                "needs `command`",
            ),
            (
                r#"{"mcpServers": {"s": {"type": "http", "command": "a"}}}"#,
                "needs `command`",
            ),
            (
                r#"{"mcpServers": {"s": {"type": "grpc", "url": "x"}}}"#,
                "unknown type",
            ),
            (
                r#"{"mcpServers": {"a__b": {"command": "a"}}}"#,
                "double-underscore",
            ),
            (r#"{"servers": {}}"#, "mcpServers"),
            ("/nonexistent/mcp.json", "can't read"),
        ] {
            let err = format!("{:#}", parse_mcp_config_arg(arg).unwrap_err());
            assert!(err.contains(want), "{arg}: {err}");
        }
    }

    #[test]
    fn debug_hides_env_and_header_values() {
        let servers = parse_mcp_config_arg(
            r#"{"mcpServers": {
                "s": {"command": "a", "env": {"TOKEN": "env-secret"}},
                "h": {"url": "https://e.com/mcp", "headers": {"Authorization": "Bearer hdr-secret"}}
            }}"#,
        )
        .unwrap();
        let out = format!("{servers:?}");
        assert!(
            out.contains("TOKEN") && out.contains("Authorization"),
            "{out}"
        );
        assert!(
            !out.contains("env-secret") && !out.contains("hdr-secret"),
            "{out}"
        );
    }
}
