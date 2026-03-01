# Architecture Design Document: Koda 🐻 (Rust AI Coding Agent)

## 1. Executive Summary
Koda 🐻 (Crate: `koda-agent`) is a high-performance, locally-focused AI coding agent built in Rust. It serves as an autonomous developer assistant capable of codebase analysis, terminal execution, and file manipulation. The Bear metaphor represents a sturdy, reliable companion that forages through your codebase with strength and precision. While the crate is named `koda-agent`, the CLI command is invoked simply as `koda`.

## 2. Technical Stack
| Category | Crate | Purpose |
|----------|-------|---------|
| Language | Rust (Edition 2024) | Core implementation |
| Async Runtime | `tokio` | Multi-threaded async execution |
| CLI Parser | `clap` | Command-line argument parsing |
| Database | `sqlx` | Async SQLite for durable state |
| Serialization | `serde`, `serde_json`, `toml` | Parsing LLM responses & configs |
| HTTP Client | `reqwest` (with `stream` feature) | LLM API calls + SSE streaming |
| Streaming | `futures-util` | Async byte stream processing |
| REPL Input | `rustyline` | Line editing, completions, hints |
| TUI Menus | `crossterm` | Arrow-key selection menus |
| Spinners | `indicatif` | Terminal progress indicators |
| Syntax Highlighting | `syntect` | Code block highlighting (bat engine) |
| Logging | `tracing`, `tracing-appender` | Invisible background file logging |
| File System | `ignore` | Respects `.gitignore` during scans |
| File Globbing | `glob` | Pattern-based file discovery |

## 3. Core Architectural Concepts

### 3.1. Single Binary CLI (Monolith)
Koda is a single compiled binary. Unlike daemon-based tools, it is invoked once for an interactive session. State persistence across terminal restarts is achieved through a local SQLite database, providing the feeling of a persistent agent without background process overhead.

### 3.2. Multi-Provider LLM Abstraction
Koda abstracts the LLM provider via the `LlmProvider` trait. It supports:
- **LM Studio** (localhost, auto-detects serving model)
- **OpenAI** (GPT-4o, o1, o3)
- **Anthropic** (Claude, via separate message format)
- **Gemini**, **Groq**, **Grok** (all OpenAI-compatible)

The provider trait exposes two methods:
- `chat()` — non-streaming request/response
- `chat_stream()` — SSE streaming via `tokio::mpsc` channel

Providers are selected interactively via `/provider` (arrow-key menu) or via CLI flags/environment variables.

### 3.3. Streaming Architecture
All LLM responses stream token-by-token via Server-Sent Events (SSE):

```
User Input → chat_stream() → tokio::mpsc::Receiver<StreamChunk>
                                    ↓
                            TextDelta("Hello")   → MarkdownStreamer → terminal
                            TextDelta(" world")  → MarkdownStreamer → terminal
                            ToolCalls([...])      → execute tools → loop
                            Done(usage)           → print footer
```

The `StreamChunk` enum:
- `TextDelta(String)` — partial text content, rendered immediately
- `ToolCalls(Vec<ToolCall>)` — accumulated tool call(s)
- `Done(TokenUsage)` — stream complete with usage stats

### 3.4. Hybrid Memory System
- **Execution Memory (SQLite — `.koda.db`):** Conversation history, tool call logs, token usage. Enables crash recovery and session resumption.
- **Semantic Memory (Markdown — `MEMORY.md`):** Project-specific rules stored as plain text. Version-controllable and human-editable. Injected into the system prompt.
- **Task Memory (`~/.config/koda/todo.md`):** Cross-project task tracking with project-scoped sections.
- **Command History (`~/.config/koda/history`):** Persistent REPL history across sessions.

### 3.5. Sub-Agent Orchestration
Koda coordinates sub-agents via standard Tool Calling:
- **`InvokeAgent` Tool:** The main LLM delegates to named sub-agents.
- **Independent Execution:** Sub-agents run in isolated loops with their own config, model, and tool access.
- **Result Coordination:** Sub-agent output is returned as a tool result string.

### 3.6. Dynamic Tool Construction
The `CreateTool` meta-tool allows the LLM (or user) to define new tools at runtime:
- **Persisted as JSON** in `agents/tools/<ToolName>.json`
- **Shell command templates** with `{{param}}` placeholders
- **Loaded automatically** on startup alongside built-in tools
- **Safety guardrails** prevent overriding built-in tools

### 3.7. Safety & Path Validation
- **Path Normalization:** `path-clean` prevents directory traversal attacks.
- **Host Execution:** Tools run as child processes with user permissions.
- **API Key Security:** Keys stored in `~/.config/koda/keys.toml` (chmod 600).

## 4. Tool System

### 4.1. Tool Naming Convention
All tools use **PascalCase** names (inspired by Claude Code):

| Tool | Module | Description |
|------|--------|-------------|
| `Read` | `file_tools` | Read file contents with optional line-range |
| `Write` | `file_tools` | Create/overwrite files |
| `Edit` | `file_tools` | Targeted find-and-replace (multi-edit) |
| `Delete` | `file_tools` | Delete a file |
| `List` | `file_tools` | List files/dirs (respects .gitignore) |
| `Grep` | `grep` | Recursive text search (regex + case-insensitive) |
| `Glob` | `glob_tool` | Find files by glob pattern |
| `Bash` | `shell` | Execute shell commands with timeout |
| `WebFetch` | `web_fetch` | Fetch URL content, strip HTML |
| `TodoRead` | `todo` | Read task list from `~/.config/koda/todo.md` |
| `TodoWrite` | `todo` | Write/update tasks (project-scoped or global) |
| `MemoryRead` | `memory` | Read project & global memory |
| `MemoryWrite` | `memory` | Save insights to persistent memory |
| `InvokeAgent` | `agent` | Delegate to a sub-agent |
| `ListAgents` | `agent` | List available sub-agents |
| `CreateTool` | `constructor` | Define a new custom tool |
| `ListTools` | `constructor` | List custom tools |
| `DeleteTool` | `constructor` | Remove a custom tool |

### 4.2. Tool Registry
Tools are registered in a `HashMap<String, ToolDefinition>` at startup. The registry:
1. Loads all built-in tool definitions from each module
2. Scans `agents/tools/` for custom tool JSON files
3. Custom tools are executed by expanding `{{param}}` placeholders in their command template and running via `Bash`

### 4.3. Tool Display
Each tool has a semantic color and short label shown during execution:

| Category | Color | Tools |
|----------|-------|-------|
| Read/navigate | Steel blue, Sky blue | `Read`, `List`, `WebFetch` |
| Modify | Amber | `Write`, `Edit`, `TodoWrite` |
| Search | Silver | `Grep`, `Glob`, `TodoRead`, `ListTools` |
| Execute | Orange | `Bash` |
| Danger | Crimson | `Delete`, `DeleteTool` |
| AI/meta | Violet, Ruby | `CreateTool`, `InvokeAgent` |

### 4.4. Confirmation System
Destructive tools require user confirmation before execution:
- `Bash`, `Delete`, `Write`, `Edit`, `CreateTool`, `DeleteTool`
- Three options: ✓ Approve, ✗ Reject, 💬 Feedback (reject with instructions)

## 5. The Core Event Loop (State Machine)
```
1. Init       → Parse CLI, load config, init SQLite
2. Prompt     → Receive user input (with completions, @file refs)
3. Pre-process → Resolve @file references, inject context
4. Context    → Assemble system prompt + sliding-window history
5. Stream     → SSE streaming with markdown rendering
6. Act        → Text → render to terminal
                 Tool calls → confirm → execute, log results
7. Loop       → Feed tool results back to step 4
```

## 6. Database Schema
```sql
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    agent_name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL, -- user, assistant, system, tool
    content TEXT,
    tool_calls TEXT, -- JSON blob of calls
    tool_call_id TEXT,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);
```

## 7. Project Directory Layout
```text
koda/
├── Cargo.toml
├── src/
│   ├── main.rs           # CLI entry point
│   ├── app.rs            # Main event loop + provider creation
│   ├── config.rs         # Agent/provider configuration
│   ├── confirm.rs        # User confirmation for destructive tools
│   ├── db.rs             # SQLite interaction layer
│   ├── display.rs        # Tool banners + response formatting
│   ├── highlight.rs      # Syntax highlighting (syntect)
│   ├── inference.rs      # Inference loop + sub-agent execution
│   ├── input.rs          # REPL completions, hints, @file refs
│   ├── interrupt.rs      # Ctrl+C handling
│   ├── keystore.rs       # API key storage (~/.config/koda/keys.toml)
│   ├── markdown.rs       # Streaming markdown renderer
│   ├── memory.rs         # Semantic memory (MEMORY.md / CLAUDE.md / AGENTS.md)
│   ├── clipboard.rs      # Copy/paste via platform commands
│   ├── context.rs        # Context window usage tracking
│   ├── onboarding.rs     # First-run setup wizard
│   ├── version.rs        # Non-blocking crates.io version check
│   ├── repl.rs           # Slash commands, banner, prompt
│   ├── tui.rs            # Arrow-key selection menus
│   ├── providers/
│   │   ├── mod.rs          # LlmProvider trait + StreamChunk
│   │   ├── openai_compat.rs # OpenAI/LM Studio/Groq/Gemini/Grok
│   │   └── anthropic.rs     # Claude Messages API
│   └── tools/
│       ├── mod.rs           # Tool registry + path safety
│       ├── file_tools.rs    # Read, Write, Edit, Delete, LS
│       ├── grep.rs          # Grep
│       ├── glob_tool.rs     # Glob
│       ├── shell.rs         # Bash
│       ├── web_fetch.rs     # WebFetch
│       ├── todo.rs          # TodoRead, TodoWrite
│       ├── agent.rs         # InvokeAgent, ListAgents
│       ├── memory.rs        # MemoryRead, MemoryWrite
│       └── constructor.rs   # CreateTool, ListTools, DeleteTool
├── tests/
│   ├── file_tools_test.rs   # File operation integration tests
│   ├── new_tools_test.rs    # Glob, WebFetch, Todo, Constructor tests
│   ├── regression_test.rs   # Command, display, naming regression tests
│   └── cli_test.rs          # Binary invocation tests (--help, --version)
└── agents/
    ├── default.json         # Default agent configuration
    └── tools/               # Custom tools (created via CreateTool)
```

## 8. Configuration & Paths

### 8.1. User-Level Config (`~/.config/koda/`)
| Path | Purpose |
|------|---------|
| `agents/` | User-level agent configs (auto-bootstrapped on first run) |
| `keys.toml` | API keys per provider (chmod 600) |
| `history` | REPL command history (rustyline) |
| `todo.md` | Shared task list with project-scoped sections |

### 8.2. Project-Level State
| Path | Purpose |
|------|---------|
| `.koda.db` | SQLite database (sessions, messages, tool calls) |
| `.koda_logs/` | Debug log files (tracing-appender) |
| `agents/` | Project-specific agent configs |
| `agents/tools/` | Custom tools (JSON, created via CreateTool) |
| `MEMORY.md` | Semantic memory (injected into system prompt) |

### 8.3. Environment Variables
| Variable | Description |
|----------|-------------|
| `KODA_BASE_URL` | Override LLM provider base URL |
| `KODA_MODEL` | Override default model name |
| `KODA_PROVIDER` | Override default provider |
| `KODA_ACCEPT_INVALID_CERTS` | Accept self-signed certs |
| `OPENAI_API_KEY` | OpenAI API key |
| `ANTHROPIC_API_KEY` | Anthropic API key |

### 8.4. Agent Config Resolution Order
1. **`<project_root>/agents/`** — repo-local agents
2. **Next to the binary** — distribution bundles
3. **`~/.config/koda/agents/`** — user-level (auto-created on first run)

The default agent JSON is embedded into the binary at compile time via `include_str!`.

## 9. Terminal Input & Display Architecture

### 9.1. Startup Banner
Two-column layout with title embedded in the top border:
```
╭── 🐻 Koda v0.1.0 ───────────────────────────────────────────────╮
│                              │ Tips for getting started          │
│   Welcome back!              │   /model      pick a model       │
│                              │   /provider   switch provider    │
│   gpt-4o                     │   /help       all commands       │
│   openai                     │ ────────────────────────────── │
│   ~/repo/koda           │ Recent activity                  │
│                              │   • what is next on the design?  │
╰──────────────────────────────────────────────────────────────────╯
```

### 9.2. Tool Display (dot + label + detail)
Each tool call is shown with a colored dot, short label, and key arguments:
```
  ● Read src/main.rs                     ← steel blue
  ● List . (recursive)                   ← sky blue
  ● Edit src/lib.rs (replace)            ← amber
  ● Search src/ for 'TODO'               ← silver
  ● Glob . → **/*.rs                     ← silver
  ● Shell $ cargo test                   ← orange
  ● Fetch https://docs.rs/...            ← sky blue
  ● Todo reading tasks                   ← silver
  ● Create tool: GitLog                  ← violet
  ● Agent frontend                       ← ruby
```

### 9.3. Streaming Markdown Renderer
Tokens are rendered with full markdown formatting as they stream:

| Element | Rendering |
|---------|-----------|
| `# Headers` | Bold cyan |
| `**bold**` | Bold |
| `*italic*` | Italic |
| `` `code` `` | Cyan with backticks |
| Code blocks | Syntax-highlighted (syntect), dim `│` border |
| `- bullets` | Cyan `•` bullet |
| `1. numbered` | Cyan number prefix |
| `> blockquotes` | Dim `│` border, italic |
| `[text](url)` | OSC 8 clickable hyperlinks |
| Tables (`\|..\|`) | Aligned columns with dim borders |
| `---` | Dim horizontal rule |

## 10. Test Coverage

239 tests across 6 suites:

| Suite | Tests | Coverage |
|-------|-------|---------|
| Unit tests (`src/`) | 172 | display, input, markdown, highlighting, tools, DB, confirm, inference, keystore, memory, clipboard, context, version, onboarding, tui |
| CLI binary integration | 3 | `--version`, `--help`, invalid flags |
| File tools integration | 17 | Path safety, CRUD, directory deletion, List filtering (target, node_modules, hidden, .git) |
| New tools integration | 22 | Glob, WebFetch, Todo sections, Constructor, naming conventions |
| Regression: commands | 7 | All slash commands dispatched, removed commands blocked, completions |
| Regression: display & flow | 18 | Tool banners (18 tools), provider key flow |
