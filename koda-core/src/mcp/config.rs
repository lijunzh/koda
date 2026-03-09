//! Configuration loading for MCP servers.
//!
//! Reads `.mcp.json` from the project root and `~/.config/koda/mcp.json`
//! for global servers. Uses the same format as Claude Code / Cursor.
//!
//! Example `.mcp.json`:
//! ```json
//! {
//!   "mcpServers": {
//!     "filesystem": {
//!       "command": "npx",
//!       "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
//!       "env": { "NODE_ENV": "production" }
//!     }
//!   }
//! }
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::tools::ToolEffect;

/// Top-level `.mcp.json` file structure.
#[derive(Debug, Deserialize, Default)]
pub struct McpConfigFile {
    /// Server configurations keyed by name.
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Tool effect overrides keyed by namespaced tool name.
    /// Example: `{ "github.create_issue": "RemoteAction" }`
    #[serde(rename = "toolOverrides", default)]
    pub tool_overrides: HashMap<String, ToolEffect>,
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// The command to run (e.g. "npx", "uvx", "python").
    pub command: String,

    /// Arguments to the command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables to set for the process.
    /// Values support `$VAR` and `${VAR}` expansion.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Timeout in seconds for tool calls (default: 30).
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Loaded MCP configuration: server configs + tool effect overrides.
pub struct McpConfigs {
    pub servers: HashMap<String, McpServerConfig>,
    pub tool_overrides: HashMap<String, ToolEffect>,
}

/// Load MCP server configs from project-level and user-level config files.
/// Project-level configs override user-level configs for the same server name.
pub fn load_mcp_configs(project_root: &Path) -> McpConfigs {
    let mut servers = HashMap::new();
    let mut tool_overrides = HashMap::new();

    // 1. User-level: ~/.config/koda/mcp.json
    if let Some(user_config) = load_user_config() {
        servers.extend(user_config.mcp_servers);
        tool_overrides.extend(user_config.tool_overrides);
    }

    // 2. Project-level: .mcp.json in project root (overrides user-level)
    if let Some(project_config) = load_project_config(project_root) {
        servers.extend(project_config.mcp_servers);
        tool_overrides.extend(project_config.tool_overrides);
    }

    // Expand environment variables in all configs
    for config in servers.values_mut() {
        expand_env_vars(config);
    }

    McpConfigs {
        servers,
        tool_overrides,
    }
}

/// Load project-level `.mcp.json`.
fn load_project_config(project_root: &Path) -> Option<McpConfigFile> {
    let path = project_root.join(".mcp.json");
    load_config_file(&path).ok()
}

/// Load user-level `~/.config/koda/mcp.json`.
fn load_user_config() -> Option<McpConfigFile> {
    let config_dir = dirs_path()?;
    let path = config_dir.join("mcp.json");
    load_config_file(&path).ok()
}

/// Read and parse a single config file.
fn load_config_file(path: &Path) -> Result<McpConfigFile> {
    if !path.exists() {
        return Ok(McpConfigFile::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read MCP config: {}", path.display()))?;
    let config: McpConfigFile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse MCP config: {}", path.display()))?;
    tracing::debug!(
        "Loaded {} MCP servers from {}",
        config.mcp_servers.len(),
        path.display()
    );
    Ok(config)
}

/// Expand `$VAR` and `${VAR}` in environment variable values.
fn expand_env_vars(config: &mut McpServerConfig) {
    for value in config.env.values_mut() {
        *value = expand_env_string(value);
    }
}

/// Expand environment variable references in a string.
fn expand_env_string(input: &str) -> String {
    let mut result = input.to_string();

    // Handle ${VAR} syntax
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            let replacement = std::env::var(var_name).unwrap_or_default();
            result = format!(
                "{}{replacement}{}",
                &result[..start],
                &result[start + end + 1..]
            );
        } else {
            break;
        }
    }

    // Handle $VAR syntax (only for remaining unresolved vars)
    let mut out = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$'
            && chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
        {
            let mut var_name = String::new();
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                var_name.push(chars.next().unwrap());
            }
            out.push_str(&std::env::var(&var_name).unwrap_or_default());
        } else {
            out.push(ch);
        }
    }

    out
}

/// Get the koda config directory path.
fn dirs_path() -> Option<std::path::PathBuf> {
    // XDG_CONFIG_HOME or ~/.config
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("koda"))
}

/// Save an MCP server config to the project-level `.mcp.json`.
pub fn save_server_to_project(
    project_root: &Path,
    name: &str,
    config: &McpServerConfig,
) -> Result<()> {
    let path = project_root.join(".mcp.json");
    let mut file_config = load_config_file(&path).unwrap_or_default();
    file_config
        .mcp_servers
        .insert(name.to_string(), config.clone());
    let json =
        serde_json::to_string_pretty(&file_config).context("Failed to serialize MCP config")?;
    std::fs::write(&path, json).with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}

/// Remove an MCP server from the project-level `.mcp.json`.
pub fn remove_server_from_project(project_root: &Path, name: &str) -> Result<bool> {
    let path = project_root.join(".mcp.json");
    let mut file_config = load_config_file(&path)?;
    let removed = file_config.mcp_servers.remove(name).is_some();
    if removed {
        let json =
            serde_json::to_string_pretty(&file_config).context("Failed to serialize MCP config")?;
        std::fs::write(&path, json)
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    Ok(removed)
}

// serde Serialize needed for save_server_to_project
impl serde::Serialize for McpConfigFile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("mcpServers", &self.mcp_servers)?;
        if !self.tool_overrides.is_empty() {
            map.serialize_entry("toolOverrides", &self.tool_overrides)?;
        }
        map.end()
    }
}

impl serde::Serialize for McpServerConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("command", &self.command)?;
        if !self.args.is_empty() {
            map.serialize_entry("args", &self.args)?;
        }
        if !self.env.is_empty() {
            map.serialize_entry("env", &self.env)?;
        }
        if let Some(t) = self.timeout {
            map.serialize_entry("timeout", &t)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_mcp_config() {
        let json = r#"{
            "mcpServers": {
                "filesystem": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                    "env": { "NODE_ENV": "production" }
                },
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"]
                }
            }
        }"#;

        let config: McpConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);

        let fs = &config.mcp_servers["filesystem"];
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args.len(), 2);
        assert_eq!(fs.env.get("NODE_ENV").unwrap(), "production");

        let gh = &config.mcp_servers["github"];
        assert_eq!(gh.command, "npx");
        assert!(gh.env.is_empty());
    }

    #[test]
    fn test_parse_empty_config() {
        let json = r#"{}"#;
        let config: McpConfigFile = serde_json::from_str(json).unwrap();
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn test_expand_env_string() {
        // Safety: test runs single-threaded, no concurrent env access
        unsafe { std::env::set_var("KODA_TEST_VAR", "hello") };
        assert_eq!(expand_env_string("$KODA_TEST_VAR"), "hello");
        assert_eq!(expand_env_string("${KODA_TEST_VAR}"), "hello");
        assert_eq!(
            expand_env_string("prefix_${KODA_TEST_VAR}_suffix"),
            "prefix_hello_suffix"
        );
        assert_eq!(expand_env_string("no_vars_here"), "no_vars_here");
        unsafe { std::env::remove_var("KODA_TEST_VAR") };
    }

    #[test]
    fn test_roundtrip_serialize() {
        let mut config = McpConfigFile::default();
        config.mcp_servers.insert(
            "test".to_string(),
            McpServerConfig {
                command: "echo".to_string(),
                args: vec!["hello".to_string()],
                env: HashMap::new(),
                timeout: Some(60),
            },
        );
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: McpConfigFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mcp_servers.len(), 1);
        assert_eq!(parsed.mcp_servers["test"].command, "echo");
    }

    #[test]
    fn test_parse_tool_overrides() {
        let json = r#"{
            "mcpServers": {},
            "toolOverrides": {
                "github.create_issue": "RemoteAction",
                "filesystem.write": "LocalMutation",
                "dangerous.drop_db": "Destructive"
            }
        }"#;

        let config: McpConfigFile = serde_json::from_str(json).unwrap();
        assert_eq!(config.tool_overrides.len(), 3);
        assert_eq!(
            config.tool_overrides["github.create_issue"],
            crate::tools::ToolEffect::RemoteAction
        );
        assert_eq!(
            config.tool_overrides["filesystem.write"],
            crate::tools::ToolEffect::LocalMutation
        );
        assert_eq!(
            config.tool_overrides["dangerous.drop_db"],
            crate::tools::ToolEffect::Destructive
        );
    }

    #[test]
    fn test_parse_config_without_overrides() {
        let json = r#"{ "mcpServers": {} }"#;
        let config: McpConfigFile = serde_json::from_str(json).unwrap();
        assert!(config.tool_overrides.is_empty());
    }
}
