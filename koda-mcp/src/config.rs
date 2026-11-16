//! MCP server configuration types.
//!
//! Servers are config: `[mcp_servers.<name>]` in `~/.config/koda/config.toml`
//! (`koda-config`), which the host reads. Earlier versions kept them in the
//! database's `kv_store` under `mcp:<name>`; `koda-client` reads those rows
//! once, to move them into the file.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Default timeout (seconds) for MCP server startup.
pub const DEFAULT_STARTUP_TIMEOUT_SEC: u64 = 30;

/// Default timeout (seconds) for individual tool calls.
pub const DEFAULT_TOOL_TIMEOUT_SEC: u64 = 120;

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
        /// The environment variable holding the bearer token for
        /// `Authorization: Bearer <token>`, as Codex's
        /// `bearer_token_env_var`. The token itself is never stored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
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
                bearer_token_env_var,
                headers,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field("bearer_token_env_var", bearer_token_env_var)
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

    /// A session doesn't start without this server: opening one fails
    /// when it fails to start, as Codex's `required`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
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
            required: false,
        }
    }

    /// Helper: build an HTTP config with defaults.
    fn http_config(url: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransport::Http {
                url: url.into(),
                bearer_token_env_var: None,
                headers: HashMap::new(),
            },
            startup_timeout_sec: 30,
            tool_timeout_sec: 120,
            enabled_tools: None,
            disabled_tools: None,
            required: false,
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
            required: true,
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
                bearer_token_env_var: Some("MY_TOKEN".into()),
                headers: HashMap::from([("X-Custom".into(), "value".into())]),
            },
            startup_timeout_sec: 15,
            tool_timeout_sec: 90,
            enabled_tools: None,
            disabled_tools: None,
            required: false,
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

    // ── Bearer tokens ────────────────────────────────────────────────────

    #[test]
    fn a_stored_server_keeps_the_variable_not_the_token() {
        let config = http_config("https://example.com/mcp");
        let McpTransport::Http { url, headers, .. } = config.transport else {
            unreachable!()
        };
        let with_var = McpServerConfig {
            transport: McpTransport::Http {
                url,
                bearer_token_env_var: Some("EXAMPLE_TOKEN".into()),
                headers,
            },
            ..http_config("x")
        };
        let json = serde_json::to_value(&with_var).unwrap();
        assert_eq!(json["bearer_token_env_var"], "EXAMPLE_TOKEN");
        // A record from before keeps loading; its token is dropped.
        let old =
            r#"{"transport": "http", "url": "https://example.com/mcp", "bearer_token": "secret"}"#;
        let parsed: McpServerConfig = serde_json::from_str(old).unwrap();
        assert!(!serde_json::to_string(&parsed).unwrap().contains("secret"));
    }

    #[test]
    fn required_is_off_unless_set() {
        let json = r#"{"transport": "stdio", "command": "npx"}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert!(!config.required);
        assert!(!serde_json::to_string(&config).unwrap().contains("required"));
    }

    #[test]
    fn debug_hides_env_and_header_values() {
        let mut stdio = stdio_config("a");
        if let McpTransport::Stdio { env, .. } = &mut stdio.transport {
            env.insert("TOKEN".into(), "env-secret".into());
        }
        let mut http = http_config("https://e.com/mcp");
        if let McpTransport::Http { headers, .. } = &mut http.transport {
            headers.insert("Authorization".into(), "Bearer hdr-secret".into());
        }
        let out = format!("{stdio:?} {http:?}");
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
