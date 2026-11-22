//! `--mcp-config`: MCP servers for one run, in the `{"mcpServers": {...}}`
//! JSON shape that Claude Code's `.mcp.json`, Qwen Code and Gemini CLI
//! share, turned into `-c mcp_servers.<name>=...` values, as Codex takes
//! a run's servers as `-c mcp_servers…` overrides. Nothing is written to
//! the file.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::{McpServerToml, Override, SERVERS_KEY};

/// One entry. Fields other agents add are ignored.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
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
struct McpJson {
    mcp_servers: BTreeMap<String, Entry>,
}

/// Parse one `--mcp-config` value, a path to a JSON file or the JSON itself
/// when it starts with `{`, into one `-c mcp_servers.<name>` value per
/// server. Each replaces a stored server of the same name for this run.
/// Every problem is an error: a flag the caller typed that silently does
/// nothing is worse than a refusal to start.
pub fn mcp_config_overrides(arg: &str) -> Result<Vec<Override>, String> {
    let (text, source) = if arg.trim_start().starts_with('{') {
        (arg.to_string(), "inline JSON".to_string())
    } else {
        let text = std::fs::read_to_string(arg)
            .map_err(|e| format!("--mcp-config: can't read {arg}: {e}"))?;
        (text, arg.to_string())
    };
    let file: McpJson = serde_json::from_str(&text).map_err(|e| {
        format!("--mcp-config: {source} isn't {{\"mcpServers\": {{...}}}} JSON: {e}")
    })?;
    file.mcp_servers
        .into_iter()
        .map(|(name, entry)| {
            check_name(&name).map_err(|e| format!("--mcp-config: {e}"))?;
            let server = entry
                .into_server(&name)
                .map_err(|e| format!("--mcp-config: {source}: {e}"))?;
            let value = toml::Value::try_from(&server)
                .map_err(|e| format!("--mcp-config: MCP server '{name}': {e}"))?;
            Ok(Override {
                key_path: format!("{SERVERS_KEY}.{name}"),
                value,
            })
        })
        .collect()
}

/// A name a key path can carry and a tool name can be routed by: letters,
/// digits, `-` and `_`, without `__`, the tool-routing separator.
fn check_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("an MCP server name must not be empty".into());
    }
    if name.contains("__") {
        return Err(format!(
            "MCP server name '{name}' must not contain `__` \
             (double-underscore is the tool-routing separator)"
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "MCP server name '{name}' must contain only letters, digits, hyphens, \
             or underscores"
        ));
    }
    Ok(())
}

impl Entry {
    fn into_server(self, name: &str) -> Result<McpServerToml, String> {
        let kind = self.kind.as_deref().map(str::to_ascii_lowercase);
        const HTTP: [&str; 3] = ["http", "streamable-http", "streamable_http"];
        let (command, url) = match (kind.as_deref(), self.command, self.url) {
            (None | Some("stdio"), Some(command), None) => (Some(command), None),
            (None, None, Some(url)) => (None, Some(url)),
            (Some(k), None, Some(url)) if HTTP.contains(&k) => (None, Some(url)),
            (Some("sse"), ..) => {
                return Err(format!(
                    "MCP server '{name}': the SSE transport isn't supported; use stdio or http"
                ));
            }
            (Some(other), ..) if other != "stdio" && !HTTP.contains(&other) => {
                return Err(format!(
                    "MCP server '{name}': unknown type '{other}'; use stdio or http"
                ));
            }
            (_, Some(_), Some(_)) => {
                return Err(format!(
                    "MCP server '{name}': give either `command` or `url`, not both"
                ));
            }
            _ => {
                return Err(format!(
                    "MCP server '{name}': needs `command` (stdio) or `url` (http) to match its type"
                ));
            }
        };
        if command.as_deref().is_some_and(|c| c.trim().is_empty()) {
            return Err(format!("MCP server '{name}': `command` is empty"));
        }
        if url.as_deref().is_some_and(|u| u.trim().is_empty()) {
            return Err(format!("MCP server '{name}': `url` is empty"));
        }
        let stdio = command.is_some();
        Ok(McpServerToml {
            command,
            args: if stdio { self.args } else { Vec::new() },
            env: if stdio { self.env } else { BTreeMap::new() },
            cwd: if stdio { self.cwd } else { None },
            url,
            bearer_token_env_var: None,
            http_headers: if stdio { BTreeMap::new() } else { self.headers },
            startup_timeout_sec: self.startup_timeout_sec,
            tool_timeout_sec: self.tool_timeout_sec,
            required: false,
            enabled_tools: self.enabled_tools,
            disabled_tools: self.disabled_tools,
            scopes: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConfigFile;

    fn servers(arg: &str) -> BTreeMap<String, McpServerToml> {
        let dir = tempfile::tempdir().unwrap();
        ConfigFile::in_dir(dir.path())
            .with_overrides(mcp_config_overrides(arg).unwrap())
            .load()
            .unwrap()
            .config
            .mcp_servers
    }

    #[test]
    fn claude_codes_shape_becomes_servers() {
        let got = servers(
            r#"{"mcpServers": {
                "fs": {"command": "npx", "args": ["-y", "fs"], "env": {"A": "1"}},
                "web": {"type": "http", "url": "http://localhost:9/mcp",
                        "headers": {"X-Key": "v"}, "toolTimeoutSec": 5},
                "bare": {"url": "https://other.example.com/mcp"}
            }}"#,
        );
        let fs = &got["fs"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, ["-y", "fs"]);
        assert_eq!(fs.env["A"], "1");
        let web = &got["web"];
        assert_eq!(web.url.as_deref(), Some("http://localhost:9/mcp"));
        assert_eq!(web.http_headers["X-Key"], "v");
        assert_eq!(web.tool_timeout_sec, Some(5));
        assert_eq!(web.command, None);
        assert_eq!(
            got["bare"].url.as_deref(),
            Some("https://other.example.com/mcp")
        );
    }

    #[test]
    fn a_file_path_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers": {"x": {"command": "run-x"}}}"#).unwrap();
        let got = servers(path.to_str().unwrap());
        assert_eq!(got["x"].command.as_deref(), Some("run-x"));
    }

    #[test]
    fn a_run_server_replaces_a_stored_one_of_the_same_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::FILE_NAME),
            "[mcp_servers.x]\nurl = \"http://stored\"\nrequired = true\n\n\
             [mcp_servers.kept]\ncommand = \"k\"\n",
        )
        .unwrap();
        let loaded = ConfigFile::in_dir(dir.path())
            .with_overrides(
                mcp_config_overrides(r#"{"mcpServers": {"x": {"command": "run-x"}}}"#).unwrap(),
            )
            .load()
            .unwrap();
        let x = &loaded.config.mcp_servers["x"];
        assert_eq!(x.command.as_deref(), Some("run-x"));
        assert_eq!(x.url, None, "nothing of the stored one is left");
        assert!(!x.required);
        assert!(loaded.config.mcp_servers.contains_key("kept"));
        assert_eq!(loaded.flag_key_paths(), ["mcp_servers.x.command"]);
    }

    #[test]
    fn bad_values_are_refused_with_the_reason() {
        for (arg, why) in [
            ("{", "isn't {\"mcpServers\""),
            (r#"{"servers": {}}"#, "mcpServers"),
            (
                r#"{"mcpServers": {"s": {"args": ["a"]}}}"#,
                "needs `command`",
            ),
            ("/no/such/file.json", "can't read"),
            (
                r#"{"mcpServers": {"a.b": {"command": "x"}}}"#,
                "only letters",
            ),
            (r#"{"mcpServers": {"a__b": {"command": "x"}}}"#, "`__`"),
            (
                r#"{"mcpServers": {"s": {"type": "sse", "url": "http://x"}}}"#,
                "SSE",
            ),
            (
                r#"{"mcpServers": {"s": {"type": "ws", "url": "http://x"}}}"#,
                "unknown type",
            ),
            (
                r#"{"mcpServers": {"s": {"command": "x", "url": "http://x"}}}"#,
                "not both",
            ),
            (
                r#"{"mcpServers": {"s": {"type": "http", "command": "x"}}}"#,
                "needs `command`",
            ),
            (r#"{"mcpServers": {"s": {"command": " "}}}"#, "empty"),
        ] {
            let err = mcp_config_overrides(arg).unwrap_err();
            assert!(err.starts_with("--mcp-config"), "{arg}: {err}");
            assert!(err.contains(why), "{arg}: {err}");
        }
    }
}
