# Koda Design Decisions

This document captures architectural decisions and their rationale.
For the TUI architecture (v0.1.2), see [#70](https://github.com/lijunzh/koda/issues/70).
For workspace structure and developer docs, see [CLAUDE.md](CLAUDE.md).

## Vision

Koda is a personal AI assistant. Coding is the starting point, but the platform
will expand to support email, messaging, calendar, reminders, documentation,
and knowledge management — all powered by the same engine.

## Execution Modes

```bash
koda                      # Auto-starts embedded engine + CLI client (default)
koda -p "fix the bug"     # Headless mode (direct engine, no server)
koda server --stdio       # ACP server over stdio (for editor integration)
```

## Design Decisions

### 1. Engine as a Library, Not a Process

**Decision**: The engine is a Rust library crate (`koda-core`) with zero IO.
It communicates exclusively through `EngineEvent` (output) and `EngineCommand`
(input) enums. See `koda-core/src/engine/event.rs` for the protocol definition.

**Rationale**: Studied four projects:
- **xi-editor**: Used stdio JSON-RPC. Discontinued. Lesson: protocol becomes
  bottleneck when core and frontend are separate processes.
- **Zed**: Keeps `agent` (engine) and `agent_ui` (rendering) as separate crates
  in the same binary. Engine has zero UI imports.
- **Goose**: Rust engine + ACP server + multiple frontends (Electron, Ink TUI, CLI).
- **Neovim**: C core + msgpack-RPC. Terminal TUI is just one client.

**Zed's approach wins**: engine and primary client in the same binary. Server
mode is optional for external clients.

### 2. ACP (Agent Client Protocol)

**Decision**: Koda's server mode will speak ACP.

**Rationale**: Both Zed and Goose independently converged on ACP
(`@agentclientprotocol/sdk`). ACP defines session management, streaming
messages, tool calls with permissions, and status updates — exactly what
Koda needs. Adopting ACP gives us Zed integration for free.

### 3. Extensibility: Thin Core + Auto-Provisioned MCP

**Decision**: The core binary contains only essential tools (file ops, shell,
search, web fetch, memory, agents). Domain-specific capabilities (AST analysis,
email, calendar, browser) are delivered as MCP servers, auto-installed on demand.

**Principle evolution**:
- **v0.1.x**: Single binary, everything compiled in.
- **v0.2.x**: Thin core + auto-provisioned capabilities.

"Everything just works" no longer means "everything compiled into one binary."
It means "everything auto-provisioned with zero user config." The user
experience is identical — zero friction — but the implementation scales to
domains beyond coding.

**How it works**: When the LLM calls a tool that isn't built-in, koda checks
an MCP capability registry, auto-installs the matching server, connects it,
and retries — transparently. The user sees a brief spinner on first use;
subsequent calls are instant.

**Rationale**: As koda expands to email, calendar, and knowledge management,
compiling every integration into one binary creates bloat. The MCP protocol
is the contract; the implementation language and deployment model are details.
AST analysis is the pilot for this pattern (see [#113](https://github.com/lijunzh/koda/issues/113)).

**MCP server language**: Default to Rust (`cargo binstall`) for koda-maintained
servers. Use Node/Python when critical libraries only exist in those ecosystems.
See [#123](https://github.com/lijunzh/koda/issues/123) for tradeoff analysis.

### 4. Async Approval Flow

**Decision**: Tool approval is an async request/response, not a blocking
function call.

**Rationale**: In server mode, the approval decision comes from a remote
client. The engine emits `EngineEvent::ApprovalRequest` and awaits
`EngineCommand::ApprovalResponse` — works identically over in-process
channels or network transport.

### 5. Database as a Monolithic Module

**Decision**: `db.rs` (~1,300 lines) stays as a single file. Do not split
into sub-modules by domain (sessions, messages, compaction, metadata).

**Rationale**: Attempted and reverted in v0.1.2. The code is tightly cohesive:
one `Database` struct, one `SqlitePool`, one `impl` block. Splitting into
`db/sessions.rs`, `db/messages.rs`, etc. added `use super::{Database, MessageRow,
Role, ...}` boilerplate to every sub-file for zero behavior change. The types,
queries, and row conversions are coupled by design (SQLite access patterns).

**Future trigger**: If v0.2.x adds genuinely new persistence domains (vector
embeddings, knowledge graph, email/calendar), those should be *new files*
alongside `db.rs` (e.g. `vector_store.rs`), not splits of the existing module.
Split by domain divergence, not by line count.

### 6. Database Backend Evolution

**Decision**: Keep SQLite for now. Introduce a `Persistence` trait so the
backend can be swapped later.

**Rationale**: SQLite is excellent for conversations, sessions, and AST cache.
But email, calendar, documents, and knowledge graphs may require full-text
search (FTS5), vector embeddings, graph relationships, or multi-device sync.
The trait boundary lets us evolve without rewriting.

### 7. Tool Dispatch: Match Statement, Not Trait Registry

**Decision**: Tools are dispatched via a `match` statement in `ToolRegistry::execute()`,
not via a `Tool` trait with dynamic dispatch.

**Rationale**: Rust's exhaustive matching catches missing tool handlers at compile
time — adding a tool without a match arm is a compile error. A `HashMap<String, Box<dyn Tool>>`
would move this to a runtime error. The match statement works well at the current
scale (~15 tools).

**Known tech debt**: Three tools (`InvokeAgent`, `TodoWrite`, `TodoRead`) return
sentinel strings (`"__INVOKE_AGENT__"`) because they need DB/session access that
the registry doesn't have. A `ToolContext` struct would fix this. Tracked in
[#122](https://github.com/lijunzh/koda/issues/122).

**Future trigger**: When tool additions become frequent enough that editing 3
locations per tool (definitions, match arm, module import) is a bottleneck,
convert to a `Tool` trait + `ToolContext`. Do both together, not piecemeal.

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
