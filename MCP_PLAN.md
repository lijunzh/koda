# MCP Support Plan for Koda

*Generated 2026-03-02 — competitive analysis + implementation plan*

---

## How the Three Competitors Implement MCP

### 🐶 Code Puppy (Python/PydanticAI)

**Architecture**: ~247KB across 18 files + 23 command files. Heavily over-engineered.

| Component | Description |
|---|---|
| **Config** | `mcp_servers.json` in XDG config dir |
| **Manager** | `MCPManager` singleton |
| **Registry** | Thread-safe CRUD, JSON persistence |
| **Transports** | SSE, Stdio, Streamable HTTP |
| **Server wrapper** | `ManagedMCPServer` with states |
| **Health** | Circuit breaker, retry, monitoring |
| **Commands** | 14 subcommands under `/mcp` |
| **Integration** | PydanticAI toolsets injected into agent |
| **Catalog** | Server discovery + install wizard |

**Key decisions**:
- Servers start **disabled** — must be explicitly started with `/mcp start`
- Uses PydanticAI's native `MCPServerStdio` / `MCPServerSSE` classes
- Env var expansion in configs (`$VAR` and `${VAR}` syntax)
- MCP tool calls get logged with a special banner
- Tool name conflict detection: filters MCP tools that collide with built-in tools
- `BlockingMCPServerStdio` for sync startup in async context

**What's good**: Comprehensive. Catalog + install wizard is slick UX.
**What's bad**: Massively over-engineered. 390KB of MCP code for what's essentially "start a subprocess and talk JSON-RPC to it." Health monitoring, circuit breakers, retry managers — YAGNI city.

### 🪿 Goose (Rust)

**Architecture**: Uses `rmcp` v0.16 crate (the official Rust MCP SDK). Well-structured.

| Component | Description |
|---|---|
| **Config** | YAML config, `ExtensionConfig` enum |
| **Client** | `McpClient` wraps `rmcp::RunningService` |
| **Manager** | `ExtensionManager` (~75KB, too big) |
| **Transports** | Stdio, Streamable HTTP (SSE deprecated) |
| **Types** | 7 extension types including `Builtin`, `Platform`, `Frontend`, `InlinePython` |
| **Security** | Env var blocklist (31 disallowed vars), malware scanning |
| **Resources** | Full MCP resources + prompts support |
| **Namespacing** | Tools prefixed with extension name to avoid collisions |

**Key decisions**:
- Uses `rmcp` crate — handles JSON-RPC, transport, handshake automatically
- `rmcp` features: `client`, `transport-child-process`, `transport-streamable-http-client`
- Extensions are MCP clients (Goose connects TO MCP servers)
- `TokioChildProcess` from rmcp handles stdio subprocess lifecycle
- Tool names are prefixed: `extensionname__toolname` (except "unprefixed" platform extensions)
- `McpClientTrait` provides mockable interface for testing
- Cancellation token support for long-running tool calls
- Stderr captured from child processes for error reporting
- Working directory forwarded to MCP server processes

**What's good**: `rmcp` handles all the hard protocol stuff. Clean Rust idioms. Good security (env var blocklist, malware check). Resource + prompt support.
**What's bad**: `extension_manager.rs` is 2000+ lines — needs splitting. Too many extension types (7) — YAGNI for a CLI tool.

### 🤖 Claude Code (TypeScript, closed source)

**Architecture**: Inferred from docs, changelog, and `.mcp.json` format.

| Component | Description |
|---|---|
| **Config** | `.mcp.json` in project root (standard MCP format) |
| **Transports** | Stdio, SSE |
| **Commands** | `/mcp` shows status |
| **Scoping** | Project-level (`.mcp.json`) + user-level (`~/.claude/`) |
| **Integration** | MCP tools appear alongside built-in tools |
| **Permission** | MCP tool calls go through permission system |

**Standard `.mcp.json` format** (de facto standard across Claude Code, Cursor, etc.):
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "NODE_ENV": "production" }
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    }
  }
}
```

**What's good**: Dead simple config. `.mcp.json` is becoming the standard that all tools share. Project-scoped.
**What's bad**: Limited to basic transports. No resource/prompt support visible.

---

## Design Decisions for Koda

### What to copy

1. **Use `rmcp` crate** (like Goose) — it handles JSON-RPC 2.0, protocol handshake, transport abstraction. Don't reinvent.
2. **Use `.mcp.json` format** (like Claude Code) — it's the de facto standard. Users can share config between tools.
3. **Tool namespacing** (like Goose) — prefix MCP tool names with `server_name.tool_name` to avoid collisions with built-in tools.
4. **Env var expansion** (like Code Puppy) — support `$VAR` in env values.
5. **Stderr capture** (like Goose) — capture MCP server stderr for debugging.

### What NOT to copy

1. **Code Puppy's health monitoring / circuit breaker / retry** — YAGNI for a CLI tool. If an MCP server dies, just report it.
2. **Code Puppy's catalog / install wizard** — Cool but scope creep. Add later.
3. **Goose's 7 extension types** — We only need `stdio` and `streamable_http`. Maybe `builtin` later.
4. **Code Puppy's 14 subcommands** — Start with 4: `/mcp` (list/status), `/mcp add`, `/mcp remove`, `/mcp restart`.
5. **Goose's malware scanning** — Interesting but out of scope for v1.

### Koda-specific design

- **Auto-connect on startup**: Read `.mcp.json` from project root + `~/.config/koda/mcp.json` for global servers. Connect automatically.
- **Servers start RUNNING**: Unlike Code Puppy (which requires explicit start), follow Claude Code's approach — servers defined in config are auto-started.
- **Tool integration**: MCP tools are injected into the existing `ToolRegistry` as additional `ToolDefinition`s. The `execute` method routes to the MCP client.
- **Permission**: MCP tool calls go through the existing approval system.

---

## Implementation Plan

### Phase 1: Core MCP Client (`src/mcp/client.rs`)

**Goal**: Connect to an MCP server via stdio, list tools, call tools.

```
src/mcp/
  mod.rs          — Public API, McpServer struct
  client.rs       — MCP client wrapper around rmcp
  config.rs       — Config loading (.mcp.json parsing)
  registry.rs     — McpRegistry: manages multiple servers
```

**Dependencies to add**:
```toml
rmcp = { version = "0.16", features = [
    "client",
    "transport-child-process",
    "transport-streamable-http-client",
    "transport-streamable-http-client-reqwest",
] }
```

**Key types**:
```rust
/// Config loaded from .mcp.json
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

/// A running MCP server connection
pub struct McpServer {
    pub name: String,
    pub config: McpServerConfig,
    client: McpClient,              // rmcp client handle
    tools: Vec<ToolDefinition>,     // Cached tool definitions
}

/// Registry of all connected MCP servers
pub struct McpRegistry {
    servers: HashMap<String, McpServer>,
}
```

**Config loading priority**:
1. `.mcp.json` in project root (project-scoped)
2. `~/.config/koda/mcp.json` (user-scoped, merged)

### Phase 2: Tool Integration (`src/tools/mod.rs` changes)

**Goal**: MCP tools appear in the tool list and can be executed.

- `ToolRegistry` gains an `McpRegistry` field
- `get_definitions()` merges built-in + MCP tools
- `execute()` routes `server_name.tool_name` calls to the MCP client
- Tool names are prefixed: `filesystem.read_file`, `github.create_issue`, etc.

### Phase 3: `/mcp` Command (`src/app.rs` additions)

**Goal**: Basic management commands.

| Command | Description |
|---|---|
| `/mcp` | Show status of all MCP servers |
| `/mcp add <name> <command> [args...]` | Add a new server (persists to config) |
| `/mcp remove <name>` | Remove a server |
| `/mcp restart [name]` | Restart one or all servers |

### Phase 4: Lifecycle Management

**Goal**: Clean startup/shutdown.

- Auto-start servers from config on session begin
- Graceful shutdown: send `shutdown` notification, wait, then kill process
- Auto-reconnect on server crash (simple: try once, then report error)
- Timeout: 30s default for tool calls, configurable per-server

---

## File Size Estimates

| File | Est. Lines | Purpose |
|---|---|---|
| `src/mcp/mod.rs` | ~50 | Re-exports, McpRegistry public API |
| `src/mcp/client.rs` | ~200 | rmcp client wrapper, tool call routing |
| `src/mcp/config.rs` | ~120 | .mcp.json parsing, env var expansion |
| `src/mcp/registry.rs` | ~250 | Multi-server management, startup/shutdown |
| **Total** | **~620** | 4 files, well under 600 each |

Plus changes to:
- `src/tools/mod.rs` — ~30 lines to integrate MCP tools
- `src/app.rs` — ~80 lines for `/mcp` command handling
- `src/repl.rs` — ~5 lines to add `/mcp` to command list
- `Cargo.toml` — 1 dependency

---

## Comparison: Koda Plan vs Competitors

| Aspect | Koda (planned) | Code Puppy | Goose | Claude Code |
|---|---|---|---|---|
| **Code size** | ~620 lines | ~6,400 lines | ~2,200 lines | Unknown |
| **Files** | 4 new | 41 files | ~5 key files | Unknown |
| **Config format** | `.mcp.json` (standard) | `mcp_servers.json` (custom) | YAML (custom) | `.mcp.json` (standard) |
| **MCP SDK** | `rmcp` 0.16 | `mcp` Python SDK | `rmcp` 0.16 | Custom |
| **Transports** | stdio + HTTP | SSE + stdio + HTTP | stdio + HTTP (SSE deprecated) | stdio + SSE |
| **Auto-start** | ✅ | ❌ (manual start) | ✅ | ✅ |
| **Tool namespacing** | `server.tool` | None (conflict filter) | `ext__tool` | Unknown |
| **Health monitoring** | ❌ (YAGNI) | ✅ (circuit breaker) | ❌ | ❌ |
| **Catalog/Install** | ❌ (later) | ✅ | ✅ | ❌ |
| **Resources** | ❌ (later) | ❌ | ✅ | ❌ |
| **Prompts** | ❌ (later) | ❌ | ✅ | ❌ |

---

## Risk Assessment

1. **`rmcp` crate maturity** — It's at v0.16, actively developed, used by Goose in production. Low risk.
2. **Tool name collisions** — Namespacing with `server.tool` prefix handles this cleanly.
3. **Subprocess lifecycle** — `rmcp`'s `TokioChildProcess` handles this. Just need clean shutdown on exit.
4. **Context window impact** — MCP tools add to the tool definition count sent to the LLM. With many MCP servers, this could eat context. Mitigation: allow `allowed_tools` filtering per-server (like Goose).
5. **Timeout handling** — Some MCP tools are slow (e.g., web search). Need per-tool or per-server timeout config.

---

## Summary

The plan is: **use `rmcp` (like Goose) + `.mcp.json` (like Claude Code) + stay lean (unlike Code Puppy)**.

~620 lines of new Rust code across 4 files to get full stdio + HTTP MCP support with auto-start, tool namespacing, and a simple `/mcp` command. No health monitoring, no catalog, no circuit breakers — just clean, working MCP that follows the standard config format.

Phase 1 (client + config) can ship independently and be tested with any MCP server.
