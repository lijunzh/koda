# MCP servers (Model Context Protocol)

Koda can connect to external **MCP servers** as a client, expanding its toolset
with anything exposed over the MCP protocol — Playwright for browser automation,
database drivers, Slack, internal APIs, and more.

## Quick start

Servers are managed from a terminal with `koda mcp`, as Codex's are with
`codex mcp`. Every new session starts the stored servers; an open TUI session
takes a change on `/mcp reconnect`.

```text
# Add a stdio server (Koda starts the command)
koda mcp add playwright -- npx -y @playwright/mcp

# Add a stdio server with environment variables
koda mcp add db-tools --env DSN=postgresql://localhost/mydb -- uvx koda-db

# Add an HTTP server whose bearer token is in $MY_API_TOKEN
koda mcp add my-api --url https://api.example.com/mcp --bearer-token-env-var MY_API_TOKEN

# List the stored servers, or show one (both take --json)
koda mcp list
koda mcp get playwright

# Remove a server
koda mcp remove playwright
```

In the TUI:

```text
/mcp              # this session's servers, their status and tools
/mcp verbose      # the same, with each server's config and tool descriptions
/mcp reconnect    # take added, removed and changed servers; retry failed ones
```

## `koda mcp`

`koda mcp add <name> -- <command> [args...]` adds a **stdio** server: Koda
spawns `<command>` with the arguments and talks to it over stdin/stdout.
`--env KEY=VALUE` (repeatable) sets environment variables for it.

`koda mcp add <name> --url <url>` adds an **HTTP** server (MCP Streamable
HTTP). `--bearer-token-env-var <VAR>` names the environment variable holding
its bearer token.

Adding a name that exists replaces that server. `list` hides environment
values (`KEY=*****`); `--json` shows the stored config.

**Name rules:** letters, digits, hyphens, and underscores only (`[a-zA-Z0-9_-]`).
Double-underscore (`__`) is reserved as Koda's internal tool-routing separator
and is not allowed.

### Bearer tokens

The token stays in the environment: Koda stores only the variable's name, as
Codex's `bearer_token_env_var` does, and reads it when the server starts. A
named variable that isn't set fails the server with that reason. Tokens stored
by Koda 0.6 (`/mcp add-http --token`) aren't read; Koda logs a warning for
such a server (see [Upgrading to 0.7](./upgrading-0.7.md#projects-and-agents)).

### Signing in with OAuth

A hosted server that signs you in through a browser (Notion, Linear, Sentry
and others) takes `koda mcp login`, as `codex mcp login` does:

```bash
koda mcp add notion --url https://mcp.notion.com/mcp
koda mcp login notion
```

Koda finds the server's authorization server, registers itself as a client
where the server allows it, and opens the sign-in page; if no browser opens,
it prints the address. When you approve, the browser comes back to a
one-time listener on `127.0.0.1` and Koda stores the tokens in
`~/.config/koda/mcp_oauth/<name>.json`, readable by you only and blocked to
the model's commands, as the API keys are. Later connections send the access
token and refresh it when it expires. Open sessions reconnect to their
servers once you're signed in.

`koda mcp logout <name>` forgets the tokens. `koda mcp list` shows each HTTP
server's auth: `OAuth`, `Bearer token`, or `-` for neither. A server that
answers 401 fails with "run `koda mcp login <name>`". A configured bearer
token wins over a sign-in. Over the protocol, `mcpServer/oauth/login` answers
with the page to open and sends `mcpServer/oauthLogin/completed` when the
browser comes back.

> **Security note:** Bearer tokens sent over plaintext `http://` are logged as
> a warning. Use `https://` in production. Koda also blocks connections to
> private/loopback addresses (SSRF protection), and follows only same-origin
> `307`/`308` redirects. A redirect to another host, port or scheme fails the
> connection instead of sending the token and custom headers there.

## `/mcp` in the TUI

`/mcp` lists the session's servers: connected, starting, failed (with the
reason) or not started, each with its tools. `/mcp verbose` adds the server's
command or URL and what each tool does. The status bar's `⚡connected/total`
pill follows the servers as they start.

`/mcp reconnect` has the open session take the stored servers again: it starts
servers added with `koda mcp add`, stops removed ones, restarts changed ones,
and retries ones that failed. During a turn, the turn keeps its servers and
the change applies when it ends. Codex's TUI has no reconnect; it gets fresh
servers from a new chat (`/new`).

`/mcp add` and `/mcp remove` print the `koda mcp` command to run.

## Tool naming

MCP tools appear in Koda's tool registry as `<server>__<tool>`. For example,
a tool named `navigate` on the `playwright` server becomes `playwright__navigate`.

The model can call these tools directly. No extra configuration is required.

## Resources

A server can offer resources: an issue, a schema, a document. Mention one
with `@server:uri`, as in Claude Code, and Koda reads it and sends it with
your message:

```text
> suggest a fix for @github:issue://123
> compare @postgres:schema://users with @docs:file://database/user-model
```

Typing `@` lists the connected servers' resources after files and skills,
matched on the URI and the resource's name. A resource's text is sent marked
as data, as a tool's result is, cut at 100,000 characters; an image is sent
as an image; other binary content is named, not sent. MCP Apps pages
(`ui://`) aren't listed. A mention whose server isn't one of the session's
connected servers, or whose part after the colon has no `://`, is read as a
file path.

## Tool filtering

A server's stored config can limit which tools it exposes. No command sets
these fields yet; a client sets them with `config/batchWrite` at
`mcp_servers.<name>`, in Codex's shape:

```json
{
  "command": "npx",
  "args": ["-y", "@playwright/mcp"],
  "enabled_tools": ["navigate", "click", "screenshot"]
}
```

`enabled_tools` is an allowlist; `disabled_tools` is a denylist. If both are
set, `enabled_tools` wins.

## Large tool lists

Every tool a server offers reaches the model, which costs context on every
turn. Koda keeps that in check as Codex does:

- **A Claude model finds MCP tools on demand.** On Claude Haiku, Sonnet and
  Opus 4.5 and later, and every Claude 5 model, MCP tools go behind the Claude
  API's [tool search](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-search-tool):
  the model sees Koda's own tools and a search tool, and loads an MCP tool's
  definition when a task needs it. The system prompt names the servers whose
  tools it can search for. Any other model, local ones included, gets every
  tool up front, as Codex's models without tool search do.
- **A large input schema is shrunk.** Past 5,000 bytes, Codex's limit, a
  tool's input schema loses, in order and only while it's still too big: its
  inner descriptions, its `$defs`, objects nested three levels or deeper, and
  `anyOf` / `oneOf` / `allOf`. Its top-level arguments always stay.

Two per-server settings, Codex's:

| Setting | Default | What it does |
|---------|---------|--------------|
| `omit_tools_from` | none | `["deferred"]` sends this server's tools up front even to a model with tool search, for a few tools the model needs often |
| `tool_input_schema_max_bytes` | 5000 | The size, in bytes of compact JSON, past which a tool's input schema is shrunk |

## Transport reference

| Transport | How it works | When to use |
|-----------|-------------|-------------|
| `stdio`   | Koda spawns a child process and communicates over stdin/stdout | Local tools, CLI-wrapped servers |
| `http`    | Koda connects to a running HTTP endpoint (MCP 2025-03-26 spec) | Remote or shared servers, cloud APIs |

Both transports start with the `initialize` handshake and offer MCP
2025-11-25. Servers on an older version (back to 2024-11-05) answer with
their own and Koda uses it. Koda doesn't use the stateless
`server/discover` lifecycle from MCP 2026-07-28 yet.

## Timeouts

| Setting | Default | Description |
|---------|---------|-------------|
| `startup_timeout_sec` | 30 s | Time allowed for the server to respond to `initialize` |
| `tool_timeout_sec` | 120 s | Time allowed for a single tool call to complete |

## Persistence

Servers live in `~/.config/koda/config.toml` as `[mcp_servers.<name>]`
tables, as in Codex's `config.toml`. `koda mcp add` and `koda mcp remove`
write them there, keeping the file's comments, and you can edit them by
hand; every new session reads the file, and `/mcp reconnect` brings open
sessions in line. `/mcp verbose` names the file each server comes from. A
server Koda can't start (both a `command` and a `url`, say) is reported at
startup and skipped. Servers that earlier versions stored in the database
move into the file on the first start.

An HTTP server's table can also carry `http_headers`, sent with every
request, as Codex's does:

```toml
[mcp_servers.docs]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "DOCS_MCP_TOKEN"
http_headers = { "X-Team" = "platform" }
```

## Startup

Each session starts its servers in the background when it opens, as Codex
does. A turn waits for any server still starting (each for up to its
`startup_timeout_sec`), so its tools include every server that connects. A
server whose config has `required: true` must start: opening the session fails
when it doesn't, so `koda exec` exits with the reason. Clients follow each
startup through the `mcpServer/startupStatus/updated` notification.

## Servers for one run (`--mcp-config`)

`--mcp-config` adds servers for one run, headless or in the TUI, without
saving them. It takes a path to a JSON file, or the JSON itself when the
value starts with `{`, in the shape Claude Code and Qwen Code use:

```json
{"mcpServers": {
  "mail":  {"command": "mail-mcp", "args": ["--read-only"], "env": {"MAIL_DIR": "./mail"}},
  "notes": {"type": "http", "url": "https://notes.example.com/mcp",
            "headers": {"Authorization": "Bearer sk-…"}}
}}
```

```bash
koda exec "Anything in my inbox?" --mcp-config mcp.json
```

- A stdio server takes `command`, and optionally `args`, `env` and `cwd`.
  An HTTP server takes `url`, and optionally `headers`; `type` (`stdio` or
  `http`) is optional. SSE servers aren't supported.
- `startupTimeoutSec`, `toolTimeoutSec`, `enabledTools` and `disabledTools`
  work as in [Timeouts](#timeouts) and [Tool filtering](#tool-filtering).
  Fields Koda doesn't know are ignored.
- The flag is repeatable. A later server wins a name clash, and a flag's
  server replaces a stored one of the same name for this run.
- The servers are this run's `-c mcp_servers.<name>` values, as Codex
  takes a run's servers, so nothing is written to `config.toml`. `/mcp`
  shows them while the run lasts.
- A value Koda can't read or use stops the start with an error, rather
  than running without the servers you asked for.

The caller names these servers for this one run, so they connect without a
prompt, as with Claude Code's and Qwen Code's `--mcp-config`. Koda doesn't
read a project's `.mcp.json`: a file in a repository someone else wrote
would need folder trust first.

## Server-supplied instructions

The MCP spec lets each server return a free-form `instructions` string in
its `initialize` response telling the model how to use it best — for
example, *"Prefer locator-based queries over CSS selectors"* for a
Playwright server, or *"Always use parameterized queries"* for a Postgres
server.

Koda surfaces these into the system prompt of every turn, in a dedicated
block that comes after Koda's own behavioral mandates:

```
# MCP Server Instructions

---[start of server instructions from playwright]---
Prefer locator-based queries over CSS selectors.
---[end of server instructions from playwright]---

---[start of server instructions from postgres]---
Always use parameterized queries.
---[end of server instructions from postgres]---
```

### What this means for users

- **Zero token cost when no MCP servers are configured.** The block is
  omitted entirely.
- **No config required.** If you've added a server with `koda mcp add`, its
  instructions surface automatically the next turn after it connects.
- **Hot-reload friendly.** A server added or removed mid-session, then
  taken with `/mcp reconnect`, updates the block in the next turn — there's
  no stale-prompt window.

### What this means for MCP server authors

- The `instructions` field in your `InitializeResult` reaches the model
  verbatim. Treat it like a short README aimed at the LLM, not at humans.
- Be concise — every byte costs tokens for every turn the user runs
  against your server.
- Don't try to override Koda's own behavioral mandates. The provenance
  framing (`---[start of server instructions from <name>]---`) makes it
  obvious to the model that your block comes from your server, not from
  Koda itself, so impersonation attempts are visible.

### Security note

Server-supplied `instructions` are untrusted content from the perspective
of the user's session — they originate from whoever runs the MCP server.
Koda passes them through verbatim (no sanitization) but wraps them in
clearly-labelled provenance markers so a malicious or compromised server
can't masquerade as Koda's own mandates.

Tool results get the same treatment: what an MCP tool returns, error text
included, reaches the model between external-content markers with a note
that it is data, not a message from you (see [Tools](tools.md#external-content)).

If you connect to MCP servers you don't fully trust, prefer the `stdio`
transport (which still flows through the OS sandbox for tool calls) and
review the `instructions` block via your model's tracing/debug output
before granting `Auto` mode.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| Server shows **failed** on `/mcp` | Check the command/URL (`koda mcp get <name>`), then `/mcp reconnect` |
| Tools not appearing after `koda mcp add` | An open session takes it on `/mcp reconnect`; a server slower than `startup_timeout_sec` fails — `/mcp reconnect` retries it |
| `__` in server name rejected | Use `-` instead: `my-server` not `my__server` |
| HTTP server SSRF error | Private/loopback URLs are blocked — use a public or VPN-accessible URL |
