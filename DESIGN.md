# Koda Architecture Design

## Overview

Koda is evolving from a single-binary CLI coding agent into a **server-backed personal AI platform**. This document captures the architectural decisions, design rationale, and implementation plan.

## Vision

Koda is a personal AI assistant. Coding is the starting point, but the platform will expand to support email, messaging, calendar, reminders, documentation, and knowledge management — all powered by the same engine.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    koda (single binary)                    │
│                                                          │
│  ┌────────────────────────────────────────────────────┐  │
│  │              koda-engine (library)                  │  │
│  │                                                    │  │
│  │  InferenceEngine  ToolRegistry  MCP  AgentSystem  │  │
│  │  SessionManager   Memory       DB   Approval      │  │
│  │                                                    │  │
│  │  Output: EngineEvent (serde-serializable enum)    │  │
│  │  Input:  EngineCommand (serde-serializable enum)  │  │
│  └──────────────┬───────────────────┬────────────────┘  │
│                 │                   │                    │
│      ┌──────────┴──────┐  ┌────────┴─────────┐         │
│      │  CLI Client     │  │  ACP Server      │         │
│      │  (rustyline)    │  │  (WebSocket)     │         │
│      │  Default mode   │  │  koda server     │         │
│      └─────────────────┘  └──────────────────┘         │
└──────────────────────────────────────────────────────────┘
         │                           │
    Terminal user              ┌─────┴──────────┐
                               │ External clients│
                               │ (Zed, VS Code, │
                               │  Desktop app)  │
                               └────────────────┘
```

## Execution Modes

```bash
koda                      # Auto-starts embedded engine + CLI client (default)
koda -p "fix the bug"     # Headless mode (direct engine, no server)
koda server               # Standalone server for external clients
koda server --port 9999   # Server on custom port
koda connect <url>        # CLI client connecting to a remote engine
```

## Design Decisions

### 1. Engine as a Library, Not a Process

**Decision**: The engine is a Rust library crate with zero IO. It communicates exclusively through `EngineEvent` (output) and `EngineCommand` (input) enums.

**Rationale**: Studied four Rust projects:
- **xi-editor**: Used stdio JSON-RPC. Discontinued. Lesson: protocol becomes bottleneck when core and frontend are separate processes.
- **Zed**: Keeps `agent` (engine) and `agent_ui` (rendering) as separate crates in the same binary. Engine has zero UI imports.
- **Goose**: Rust engine + ACP server + multiple frontends (Electron, Ink TUI, CLI).
- **Neovim**: C core + msgpack-RPC. Terminal TUI is just one client.

**Zed's approach wins**: engine and primary client in the same binary. Server mode is optional for external clients.

### 2. ACP (Agent Client Protocol)

**Decision**: Koda's server mode will speak ACP.

**Rationale**: Both Zed and Goose independently converged on ACP (`@agentclientprotocol/sdk`). ACP defines session management, streaming messages, tool calls with permissions, and status updates — exactly what Koda needs. Adopting ACP gives us Zed integration for free.

### 3. Single Binary Philosophy

**Decision**: `cargo install koda-agent` gives you everything. No separate server process required for normal usage.

**Rationale**: Koda's core value is zero-config simplicity. The CLI client talks to the engine via in-process `tokio::mpsc` channels. Server mode is opt-in (`koda server`) for external clients.

### 4. Async Approval Flow

**Decision**: Tool approval is an async request/response, not a blocking function call.

**Rationale**: The current `confirm::confirm_tool_action()` blocks the inference loop to show a terminal select widget. In server mode, the approval decision comes from a remote client. The engine must emit `EngineEvent::ApprovalRequest` and await `EngineCommand::ApprovalResponse`.

### 5. Database Evolution

**Decision**: Keep SQLite for v0.2.0. Introduce a `Persistence` trait so the backend can be swapped later.

**Rationale**: SQLite is excellent for conversations, sessions, and AST cache. But email, calendar, documents, and knowledge graphs may require full-text search (FTS5), vector embeddings, graph relationships, or multi-device sync. The trait boundary lets us evolve without rewriting.

## Protocol: EngineEvent / EngineCommand

### EngineEvent (Engine → Client)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EngineEvent {
    // Streaming LLM output
    TextDelta { text: String },
    TextDone,
    ThinkingStart,
    ThinkingDelta { text: String },
    ThinkingDone,
    ResponseStart,

    // Tool execution
    ToolCallStart { id: String, name: String, args: Value },
    ToolCallResult { id: String, output: String, success: bool },

    // Interactive
    ApprovalRequest { id: String, tool: String, detail: String, preview: Option<String> },

    // Session metadata
    StatusUpdate { model: String, context_pct: f64, mode: String },
    Footer { tokens: i64, time_ms: u64, rate: f64, context: String },
    Info(String),
    Warn(String),
    Error(String),
}
```

### EngineCommand (Client → Engine)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EngineCommand {
    UserPrompt { text: String, images: Vec<ImageData> },
    Interrupt,
    ApprovalResponse { id: String, decision: ApprovalDecision },
    Command(SlashCommand),
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum ApprovalDecision {
    Approve,
    Reject,
    RejectWithFeedback(String),
    AlwaysAllow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum SlashCommand {
    Compact,
    SwitchModel { model: String },
    SwitchProvider { provider: String },
    ListSessions,
    DeleteSession { id: String },
    SetTrust { mode: String },
    McpCommand { args: String },
    Cost,
    Memory { action: Option<String> },
}
```

## Implementation Phases

See GitHub Issues for tracking:
- **Phase 1** (v0.1.4): Engine extraction — #39, #40, #41
- **Phase 2** (v0.2.0): Server mode — #38
- **Phase 3** (v0.2.x): External clients

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
