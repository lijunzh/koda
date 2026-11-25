//! `koda mcp list|get|add|remove`: manage the MCP servers in
//! `~/.config/koda/config.toml` from the shell, as `codex mcp` does
//! (`codex-rs/cli/src/mcp_cmd.rs`): the same subcommands, flags and output.
//! These go through the host's `config/read` and `config/batchWrite`, which
//! keep the file's comments; the file can also be edited by hand. Open
//! sessions take a change on `/mcp reconnect`.
//!
//! `login` and `logout` sign in to a streamable HTTP server with OAuth, as
//! `codex mcp login` does, through the host's `mcpServer/oauth/login`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use koda_client::protocol::config::{
    ConfigBatchWriteParams, ConfigEdit, ConfigReadParams, MergeStrategy,
};
use koda_client::protocol::initialize::ClientInfo;
use koda_client::protocol::mcp::McpServerConfigValue;
use koda_client::protocol::methods;
use koda_client::{InProcessClient, StartArgs};
use serde_json::Value;

/// Manage the MCP servers every session starts.
#[derive(Debug, clap::Args)]
pub(crate) struct McpCli {
    #[command(subcommand)]
    subcommand: McpSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum McpSubcommand {
    /// List the configured servers.
    List(ListArgs),
    /// Show one server.
    Get(GetArgs),
    /// Add a server, or replace one of the same name.
    Add(AddArgs),
    /// Remove a server.
    Remove(RemoveArgs),
    /// Sign in to a streamable HTTP server with OAuth.
    Login(LoginArgs),
    /// Forget a server's OAuth sign-in.
    Logout(LogoutArgs),
}

#[derive(Debug, clap::Args)]
struct LoginArgs {
    /// Name of the MCP server to sign in to.
    name: String,
    /// Comma-separated list of OAuth scopes to request.
    /// Without it: the server's `scopes` in config.toml, or else every scope
    /// the server advertises.
    #[arg(long, value_delimiter = ',', value_name = "SCOPE,SCOPE")]
    scopes: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct LogoutArgs {
    /// Name of the MCP server to sign out of.
    name: String,
}

#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Output the configured servers as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct GetArgs {
    /// Name of the MCP server to display.
    name: String,
    /// Output the server configuration as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
#[command(override_usage = "koda mcp add [OPTIONS] <NAME> (--url <URL> | -- <COMMAND>...)")]
struct AddArgs {
    /// Name for the MCP server configuration.
    name: String,
    #[command(flatten)]
    transport: AddTransportArgs,
}

#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("transport")
        .args(["command", "url"])
        .required(true)
        .multiple(false)
))]
struct AddTransportArgs {
    #[command(flatten)]
    stdio: Option<AddStdioArgs>,
    #[command(flatten)]
    http: Option<AddHttpArgs>,
}

#[derive(Debug, clap::Args)]
struct AddStdioArgs {
    /// Command to launch the MCP server.
    /// Use --url for a streamable HTTP server.
    #[arg(trailing_var_arg = true, num_args = 0..)]
    command: Vec<String>,
    /// Environment variables to set when launching the server.
    /// Only valid with stdio servers.
    #[arg(long, value_parser = parse_env_pair, value_name = "KEY=VALUE")]
    env: Vec<(String, String)>,
}

#[derive(Debug, clap::Args)]
struct AddHttpArgs {
    /// URL for a streamable HTTP MCP server.
    #[arg(long)]
    url: String,
    /// Optional environment variable to read for a bearer token.
    /// Only valid with streamable HTTP servers.
    #[arg(
        long = "bearer-token-env-var",
        value_name = "ENV_VAR",
        requires = "url"
    )]
    bearer_token_env_var: Option<String>,
}

#[derive(Debug, clap::Args)]
struct RemoveArgs {
    /// Name of the MCP server configuration to remove.
    name: String,
}

fn parse_env_pair(raw: &str) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| "environment entries must be in KEY=VALUE form".to_string())?;
    let key = key.trim();
    if key.is_empty() {
        return Err("environment entries must be in KEY=VALUE form".to_string());
    }
    Ok((key.to_string(), value.to_string()))
}

impl McpCli {
    /// Run the subcommand against a host.
    pub(crate) async fn run(self, project_root: PathBuf) -> Result<()> {
        let mut client = InProcessClient::start(StartArgs {
            config_file: koda_tui::user_config::file()?,
            home: koda_config::config_dir()?,
            project_root,
            client_info: ClientInfo {
                name: "koda-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
            },
        })
        .await?;
        let result = match self.subcommand {
            McpSubcommand::List(args) => list(&client, args).await,
            McpSubcommand::Get(args) => get(&client, args).await,
            McpSubcommand::Add(args) => add(&client, args).await,
            McpSubcommand::Remove(args) => remove(&client, args).await,
            McpSubcommand::Login(args) => login(&mut client, args).await,
            McpSubcommand::Logout(args) => logout(&client, args).await,
        };
        client.shutdown().await;
        result
    }
}

async fn servers(client: &InProcessClient) -> Result<BTreeMap<String, McpServerConfigValue>> {
    let read = client
        .request::<methods::ConfigRead>(ConfigReadParams::default())
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(read.config.mcp_servers)
}

async fn write(client: &InProcessClient, name: &str, value: Value) -> Result<()> {
    client
        .request::<methods::ConfigBatchWrite>(ConfigBatchWriteParams {
            edits: vec![ConfigEdit {
                key_path: format!("{}.{name}", koda_client::protocol::mcp::SERVERS_KEY),
                value,
                merge_strategy: MergeStrategy::Replace,
            }],
            expected_version: None,
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

async fn add(client: &InProcessClient, args: AddArgs) -> Result<()> {
    let AddArgs { name, transport } = args;
    let value = match transport {
        AddTransportArgs {
            stdio: Some(stdio), ..
        } => {
            let mut parts = stdio.command.into_iter();
            let command = parts.next().ok_or_else(|| anyhow!("command is required"))?;
            McpServerConfigValue {
                command: Some(command),
                args: parts.collect(),
                env: stdio.env.into_iter().collect(),
                ..McpServerConfigValue::default()
            }
        }
        AddTransportArgs {
            http: Some(http), ..
        } => McpServerConfigValue {
            url: Some(http.url),
            bearer_token_env_var: http.bearer_token_env_var,
            ..McpServerConfigValue::default()
        },
        _ => bail!("exactly one of --command or --url must be provided"),
    };
    write(client, &name, serde_json::to_value(value)?).await?;
    println!("Added MCP server '{name}'.");
    Ok(())
}

async fn remove(client: &InProcessClient, args: RemoveArgs) -> Result<()> {
    let name = args.name;
    if servers(client).await?.contains_key(&name) {
        write(client, &name, Value::Null).await?;
        println!("Removed MCP server '{name}'.");
    } else {
        println!("No MCP server named '{name}' found.");
    }
    Ok(())
}

/// Sign in: the page to open, opened if a browser can be, then the wait for
/// the host to say the sign-in finished.
async fn login(client: &mut InProcessClient, args: LoginArgs) -> Result<()> {
    use koda_client::events::Outgoing;
    use koda_client::protocol::ServerNotification;
    use koda_client::protocol::mcp::McpServerOauthLoginParams;
    let name = args.name;
    let started = client
        .request::<methods::McpServerOauthLogin>(McpServerOauthLoginParams {
            name: name.clone(),
            scopes: (!args.scopes.is_empty()).then_some(args.scopes),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let url = started.authorization_url;
    println!("Sign in to '{name}' in your browser. If it doesn't open, go to:\n\n  {url}\n");
    open_browser(&url);
    while let Some(message) = client.next_event().await {
        if let Outgoing::Notification(ServerNotification::McpServerOauthLoginCompleted(done)) =
            message
            && done.name == name
        {
            if done.success {
                println!("Signed in to '{name}'.");
                return Ok(());
            }
            bail!(
                "{}",
                done.error
                    .unwrap_or_else(|| format!("the sign-in to '{name}' failed"))
            );
        }
    }
    bail!("Koda's host stopped before the sign-in finished")
}

async fn logout(client: &InProcessClient, args: LogoutArgs) -> Result<()> {
    use koda_client::protocol::mcp::McpServerOauthLogoutParams;
    let name = args.name;
    let answer = client
        .request::<methods::McpServerOauthLogout>(McpServerOauthLogoutParams { name: name.clone() })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    if answer.removed {
        println!("Signed out of '{name}'.");
    } else {
        println!("No OAuth sign-in stored for '{name}'.");
    }
    Ok(())
}

/// Open `url` in the default browser, if there is one; the URL is printed
/// either way.
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// A server as JSON: its name, then its config.
fn json_entry(name: &str, value: &McpServerConfigValue) -> Result<Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("name".to_string(), Value::String(name.to_string()));
    if let Value::Object(fields) = serde_json::to_value(value)? {
        entry.extend(fields);
    }
    Ok(Value::Object(entry))
}

/// Environment variable names with their values hidden, as Codex shows them.
fn env_display<V>(env: &BTreeMap<String, V>) -> String {
    if env.is_empty() {
        "-".to_string()
    } else {
        env.keys()
            .map(|key| format!("{key}=*****"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

async fn list(client: &InProcessClient, args: ListArgs) -> Result<()> {
    let servers = servers(client).await?;
    if args.json {
        let entries = servers
            .iter()
            .map(|(name, value)| json_entry(name, value))
            .collect::<Result<Vec<_>>>()?;
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if servers.is_empty() {
        println!("No MCP servers configured yet. Try `koda mcp add my-tool -- my-command`.");
        return Ok(());
    }
    let auth = auth_statuses(client).await;
    let mut stdio = Vec::new();
    let mut http = Vec::new();
    for (name, value) in &servers {
        match (&value.command, &value.url) {
            (Some(command), _) => stdio.push(vec![
                name.clone(),
                command.clone(),
                if value.args.is_empty() {
                    "-".to_string()
                } else {
                    value.args.join(" ")
                },
                env_display(&value.env),
                value.cwd.clone().unwrap_or_else(|| "-".to_string()),
            ]),
            (None, Some(url)) => http.push(vec![
                name.clone(),
                url.clone(),
                value
                    .bearer_token_env_var
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                auth.get(name).cloned().unwrap_or_else(|| "-".to_string()),
            ]),
            (None, None) => {}
        }
    }
    if !stdio.is_empty() {
        print_table(&["Name", "Command", "Args", "Env", "Cwd"], &stdio);
    }
    if !stdio.is_empty() && !http.is_empty() {
        println!();
    }
    if !http.is_empty() {
        print_table(&["Name", "Url", "Bearer Token Env Var", "Auth"], &http);
    }
    Ok(())
}

/// Each configured server's auth, as Codex's list shows it.
async fn auth_statuses(client: &InProcessClient) -> BTreeMap<String, String> {
    use koda_client::protocol::mcp::{ListMcpServerStatusParams, McpAuthStatus};
    let Ok(listed) = client
        .request::<methods::McpServerStatusList>(ListMcpServerStatusParams::default())
        .await
    else {
        return BTreeMap::new();
    };
    listed
        .data
        .into_iter()
        .filter_map(|server| {
            let label = match server.auth_status? {
                McpAuthStatus::Unsupported => "Unsupported",
                McpAuthStatus::BearerToken => "Bearer token",
                McpAuthStatus::OAuth => "OAuth",
                McpAuthStatus::Unknown => "-",
            };
            Some((server.name, label.to_string()))
        })
        .collect()
}

fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: Vec<&str>| {
        let text = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{}", text.trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}

async fn get(client: &InProcessClient, args: GetArgs) -> Result<()> {
    let servers = servers(client).await?;
    let Some(value) = servers.get(&args.name) else {
        bail!("No MCP server named '{}' found.", args.name);
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_entry(&args.name, value)?)?
        );
        return Ok(());
    }
    let dash = |v: Option<&str>| v.unwrap_or("-").to_string();
    println!("{}", args.name);
    println!("  required: {}", value.required);
    if let Some(command) = &value.command {
        println!("  transport: stdio");
        println!("  command: {command}");
        println!(
            "  args: {}",
            if value.args.is_empty() {
                "-".to_string()
            } else {
                value.args.join(" ")
            }
        );
        println!("  cwd: {}", dash(value.cwd.as_deref()));
        println!("  env: {}", env_display(&value.env));
    } else if let Some(url) = &value.url {
        println!("  transport: streamable_http");
        println!("  url: {url}");
        println!(
            "  bearer_token_env_var: {}",
            dash(value.bearer_token_env_var.as_deref())
        );
        println!("  http_headers: {}", env_display(&value.http_headers));
        if let Some(scopes) = &value.scopes {
            println!("  scopes: {}", scopes.join(", "));
        }
    }
    if let Some(secs) = value.startup_timeout_sec {
        println!("  startup_timeout_sec: {secs}");
    }
    if let Some(secs) = value.tool_timeout_sec {
        println!("  tool_timeout_sec: {secs}");
    }
    if let Some(tools) = &value.enabled_tools {
        println!("  enabled_tools: {}", tools.join(", "));
    }
    if let Some(tools) = &value.disabled_tools {
        println!("  disabled_tools: {}", tools.join(", "));
    }
    println!("  remove: koda mcp remove {}", args.name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_pairs_need_a_key() {
        assert_eq!(
            parse_env_pair("A=b=c").unwrap(),
            ("A".to_string(), "b=c".to_string())
        );
        assert!(parse_env_pair("=x").is_err());
        assert!(parse_env_pair("novalue").is_err());
    }

    #[test]
    fn env_values_are_hidden() {
        let env = BTreeMap::from([("TOKEN".to_string(), "secret".to_string())]);
        assert_eq!(env_display(&env), "TOKEN=*****");
        assert_eq!(env_display(&BTreeMap::<String, String>::new()), "-");
    }
}
