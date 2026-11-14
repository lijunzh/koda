//! `koda mcp list|get|add|remove`: manage the stored MCP servers from the
//! shell, as `codex mcp` does (`codex-rs/cli/src/mcp_cmd.rs`): the same
//! subcommands, flags and output. Codex's CLI writes `config.toml` itself;
//! Koda's store is the host's database, so these go through the host's
//! `config/read` and `config/batchWrite`. Open sessions take a change on
//! `/mcp reconnect`.
//!
//! Left out: `login` and `logout`, until Koda has MCP OAuth.

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
use koda_core::config::KodaConfig;
use koda_core::db::Database;
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
    /// Run the subcommand against a host on `db`.
    pub(crate) async fn run(
        self,
        config: KodaConfig,
        project_root: PathBuf,
        db: Database,
    ) -> Result<()> {
        let client = InProcessClient::start(StartArgs {
            config,
            project_root,
            db,
            raw_events: false,
            client_info: ClientInfo {
                name: "koda-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            agent: None,
        })
        .await?;
        let result = match self.subcommand {
            McpSubcommand::List(args) => list(&client, args).await,
            McpSubcommand::Get(args) => get(&client, args).await,
            McpSubcommand::Add(args) => add(&client, args).await,
            McpSubcommand::Remove(args) => remove(&client, args).await,
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
        print_table(&["Name", "Url", "Bearer Token Env Var"], &http);
    }
    Ok(())
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
