# MCP servers (Model Context Protocol)

Koda can connect to external **MCP servers** as a client, expanding its toolset
with anything exposed over the MCP protocol — Playwright for browser automation,
database drivers, Slack, internal APIs, and more.

## Quick start

```text
# Add a stdio server (spawns a child process)
/mcp add playwright npx -y @playwright/mcp

# Add an HTTP server
/mcp add-http my-api https://api.example.com/mcp

# Add an HTTP server with bearer-token auth
/mcp add-http my-api https://api.example.com/mcp --token sk-mytoken

# List all configured servers and their status
/mcp list

# Reconnect a server without restarting Koda
/mcp reconnect playwright

# Remove a server
/mcp remove playwright
```

## Slash commands

### `/mcp list`

Shows every configured MCP server, its transport (stdio / http), and
its current connection status (connected / failed / disconnected).

### `/mcp add <name> <command> [args...]`

Add a **stdio** MCP server. Koda spawns `<command>` with the given
arguments and communicates over stdin/stdout.

```text
/mcp add playwright  npx -y @playwright/mcp
/mcp add db-tools    uvx koda-db --dsn postgresql://localhost/mydb
```

**Name rules:** letters, digits, hyphens, and underscores only (`[a-zA-Z0-9_-]`).
Double-underscore (`__`) is reserved as Koda's internal tool-routing separator
and is not allowed.

### `/mcp add-http <name> <url> [--token <bearer>]`

Add an **HTTP** MCP server using the MCP 2025-03-26 Streamable HTTP transport.

```text
/mcp add-http my-api  https://api.example.com/mcp
/mcp add-http my-api  https://api.example.com/mcp --token sk-abc123
```

> **Security note:** Bearer tokens sent over plaintext `http://` are logged as
> a warning. Use `https://` in production. Koda also blocks connections to
> private/loopback addresses (SSRF protection).

### `/mcp reconnect <name>`

Disconnect and re-connect a running server without restarting Koda. Useful
after updating the server binary or rotating credentials.

#/mcp remove <name>`

Permanently delete the server config from the local keystore and disconnect it
immediately. The server process (if stdio) is cleaned up automatically.

## Tool naming

MCP tools appear in Koda's tool registry as `<server>__<tool>`. For example,
a tool named `navigate` on the `playwright` server becomes `playwright__navigate`.

The model can call these tools directly. No extra configuration is required.

## Tool filtering

To limit which tools a server exposes, edit `~/.config/koda/mcp.json` manually
or use `koda config` (future release). Example snippet:

```json
{
  "playwright": {
    "transport": "stdio",
    "command": "npx",
    "args": ["-y", "@playwright/mcp"],
    "enabled_tools": ["navigate", "click", "screenshot"]
  }
}
```

`enabled_tools` is an allowlist; `disabled_tools` is a denylist. If both are
set, `enabled_tools` wins.

## Transport reference

| Transport | How it works | When to use |
|-----------|-------------|-------------|
| `stdio`   | Koda spawns a child process and communicates over stdin/stdout | Local tools, CLI-wrapped servers |
| `http`    | Koda connects to a running HTTP endpoint (MCP 2025-03-26 spec) | Remote or shared servers, cloud APIs |

## Timeouts

| Setting | Default | Description |
|---------|---------|-------------|
| `startup_timeout_sec` | 30 s | Time allowed for the server to respond to `initialize` |
| `tool_timeout_sec` | 120 s | Time allowed for a single tool call to complete |

## Persistence

MCP server configs are stored in the local SQLite keystore (`~/.local/share/koda/koda.db`)
under the `mcp:` key prefix. They survive session restarts and are loaded
automatically on each startup.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| Server shows **failed** on `/mcp list` | Check the command/URL, then `/mcp reconnect <name>` |
| Tools not appearing after add | The server may have taken longer than `startup_timeout_sec` to start — `/mcp reconnect` or restart Koda |
| `__` in server name rejected | Use `-` instead: `my-server` not `my__server` |
| HTTP server SSRF error | Private/loopback URLs are blocked — use a public or VPN-accessible URL |
