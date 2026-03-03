# Koda Roadmap

Future features and competitive positioning.

**Koda's core strength:** Single compiled Rust binary, multi-provider LLM
support, zero runtime dependencies. No other agent matches that deployment story.

**What shipped in v0.1.x:** Core file/shell/search tools (15), 6 LLM providers,
5 embedded agents, parallel tool execution, streaming markdown, image analysis,
session management, auto-memory, context compression, headless/CI mode, prompt
caching, `/diff` review, clipboard integration, onboarding wizard, MCP protocol
support (`.mcp.json`), capabilities self-discovery, improved `/compact`.
See [CHANGELOG.md](CHANGELOG.md) for details.

---

## v0.1.x — Remaining Extensibility & Security

### MCP Protocol ✅ Shipped

MCP support shipped with stdio transport, `.mcp.json` config (compatible with
Claude Code / Cursor), auto-connect on startup, tool namespacing, and `/mcp`
command (status, add, remove, restart).

**Remaining for later:**
- Streamable HTTP transport (for remote MCP servers)
- MCP resources and prompts support
- MCP server mode (expose Koda's tools to other agents)

### Per-Tool Permission System

**What:** Granular permissions: `always_allow`, `ask_before`, `never_allow`
per tool name. Stored in `~/.config/koda/permissions.toml`.

**Why:** Current binary approve/reject is too coarse. Users should be able
to auto-approve `Read`/`List`/`Grep` while always confirming `Bash`/`Delete`.
Goose and Claude Code both have this.

### Plugin / Hook System

**What:** Allow users to extend Koda without forking. Hooks at key
lifecycle points:
- `pre_tool_call` / `post_tool_call`
- `on_edit_file` / `on_shell_command`
- `on_startup` / `on_shutdown`

**Approach:** Start with JSON/TOML config pointing to shell scripts
(lightweight, no new runtime). Consider WASM plugins later.

---

## v0.2.0 — TUI & Multi-Model

### Type-Ahead Prompting (Top Priority for v0.2.0)

**What:** Let the user type their next prompt while Koda is still executing
the previous one. Queued prompts are fed to the LLM after the current
inference cycle completes.

**Why:** Currently, Koda blocks the input line during execution. If you
think of a follow-up while watching tool output stream, you have to wait.
This breaks flow, especially on long multi-tool tasks.

**Requirements:**
- Input line must be active at all times (non-blocking)
- Typed text must not interleave with streaming output
- Queued prompts execute in order after current inference finishes
- `Ctrl+C` during typing should clear the input, not kill the running task
- `Ctrl+C` twice (or `Esc`) should interrupt the running task

**Architecture:** Requires separating the input loop from the execution
loop into two concurrent tokio tasks communicating via `mpsc` channels.
This is the same foundation needed for the full TUI.

### Concurrent TUI with Non-Blocking Input

**Vision:** Separate the input loop from the execution loop so the user
can type new prompts while Koda is thinking or executing tools.

**Architecture:**
```
┌──────────────────────────────────────────────────────┐
│  Scrollable output area                              │
│  (streaming LLM response, tool output, agent results) │
├──────────────────────────────────────────────────────┤
│ > type your next prompt here...                       │
├──────────────────────────────────────────────────────┤
│ /help · claude-sonnet-4 · ctx: 12% · ⚡ 2 tasks running │
└──────────────────────────────────────────────────────┘
```

**Three regions:**
1. **Output area** (scrollable) — streaming responses, tool banners
2. **Input line** (always active) — type while output is streaming
3. **Footer bar** (persistent) — model, context %, active task count

**Implementation:** `ratatui` + `crossterm` (already a dependency).
Two tokio tasks communicating via `mpsc` channels.

**Migration path (incremental):**
1. Persistent footer bar (model, context %, shortcuts)
2. Separate input into its own tokio task
3. Scrollable output area (full ratatui alternate screen)
4. Parallel task queue with collapsible task groups

### Lead-Worker Multi-Model

**What:** Use a powerful model (e.g., Claude Sonnet) for initial planning
turns, then switch to a cheaper model (e.g., GPT-4o-mini) for execution.
Goose has this; it saves significant cost on long sessions.

### More Providers

Priority additions: Ollama, OpenRouter, Azure OpenAI, AWS Bedrock.
These cover most enterprise and local-first use cases.

---

## Future (Unscheduled)

Ideas tracked but not yet prioritized. Will be scheduled based on
real usage patterns and demand.

### IDE Integration (VS Code Extension)
VS Code extension communicating with Koda via IPC/WebSocket.
Goose and Claude Code both have this.

### Agent Teams — Git Worktree Isolation
Parallel WRITE operations via git worktrees. Each agent gets its own
checkout, branches merge after completion. Phase 1 (parallel read-only)
is already shipped. Phase 2 only matters when users need parallel writes.

### Browser Automation
Playwright-based browser control for web testing, scraping, and UI
interaction. Code Puppy and Goose (ComputerController) have this.

### Tree-Sitter Code Analysis
AST-based code analysis with call graphs and symbol extraction.
Goose's Analyze extension supports 11 languages via tree-sitter.
Would replace text-based grep for structural code understanding.

### Document Parsing
PDF, DOCX, XLSX reading. Goose has this via ComputerController.
Low priority — most coding agents don't need office documents.

### Scheduled Tasks
Cron-based agent execution. Goose has `tokio-cron-scheduler`.
Only useful once Koda has a server/daemon mode.

### Skills / Recipe System
Declarative YAML workflows with parameters and sub-tasks.
Goose's recipe system is a differentiator. Worth considering
once MCP and permissions are solid.

---

## Security Audit Findings — 2025-04-01 ✅

**Status:** Documented.

### RUSTSEC-2023-0071 (Medium severity 5.9)

- **Crate:** `rsa` v0.9.10 (transitive dependency)
- **Issue:** Marvin Attack — potential key recovery through timing sidechannels
- **Affected dependency chain:** sqlx → sqlx-macros → sqlx-macros-core → sqlx-mysql
- **Solution:** No fixed upgrade available yet

**Impact Assessment:**
- Koda primarily uses SQLite (WAL mode) for local storage
- MySQL support is available via `--base-url` but not the primary use case
- The attack requires cryptographic timing sidechannels, which is unlikely in this CLI agent's threat model
- This is a low-priority finding for the current architecture

**Decision:** Monitor for upstream fix. No immediate action required.

### Additional Warnings (Low Priority)

**RUSTSEC-2025-0141: `bincode` is unmaintained**

- **Crate:** bincode v1.3.3
- **Dependency chain:** syntect (markdown highlighting)
- **Assessment:** Stable crate, no security vulnerabilities. Replacement `bincode2` exists but not required.

**RUSTSEC-2025-0119: `number_prefix` is unmaintained**

- **Crate:** number_prefix v0.4.0
- **Dependency chain:** indicatif (progress bars)
- **Assessment:** Stable crate with no known issues. Used for human-readable size formatting.

**Overall Assessment:** All warnings are low-priority. Koda's zero-dependency binary architecture remains intact.

---

## Competitive Summary

### Core Agent Capabilities

| Capability | Koda v0.1.1 | Goose | Claude Code | Code Puppy |
|------------|:-----------:|:-----:|:-----------:|:----------:|
| Core file/shell/search | ✅ | ✅ | ✅ | ✅ |
| Multi-provider LLM | ✅ (6) | ✅ (25+) | ❌ (1) | ✅ (65+) |
| Streaming + markdown | ✅ | ✅ | ✅ | ✅ |
| Sub-agent delegation | ✅ | ✅ | ✅ | ✅ |
| Dynamic tool creation | ✅ | ❌ | ❌ | ✅ |
| Proxy support | ✅ | ✅ | ❌ | ✅ |
| Zero-dependency binary | ✅ | ❌ | ❌ | ❌ |
| Image analysis | ✅ | ✅ | ✅ | ✅ |
| Session management | ✅ | ✅ | ✅ | ✅ |
| Auto-memory | ✅ | ✅ (goosehints) | ✅ | ❌ |
| Context compression | ✅ (auto 80%) | ✅ (configurable) | ✅ | ❌ |
| Headless/CI mode | ✅ | ✅ | ✅ | ✅ |
| Prompt caching | ✅ | ✅ | ✅ | ✅ |
| Agent teams (parallel) | ✅ (Phase 1) | ✅ | ✅ | ✅ |

### Extensibility & Ecosystem

| Capability | Koda v0.1.1 | Goose | Claude Code | Code Puppy |
|------------|:-----------:|:-----:|:-----------:|:----------:|
| MCP protocol | ✅ | ✅ (70+ servers) | ✅ | ✅ |
| Plugin/hook system | ❌ | ✅ (extensions) | ✅ | ✅ |
| Recipe/workflow system | ❌ | ✅ (YAML + sub-recipes) | ❌ | ❌ |
| Skills/marketplace | ❌ | ✅ (recipe catalog) | ❌ | ✅ |
| Custom distributions | ❌ | ✅ (white-label) | ❌ | ❌ |
| Declarative providers | ❌ | ✅ (JSON configs) | ❌ | ❌ |

### Interfaces & UX

| Capability | Koda v0.1.1 | Goose | Claude Code | Code Puppy |
|------------|:-----------:|:-----:|:-----------:|:----------:|
| CLI REPL | ✅ | ✅ | ✅ | ✅ |
| Desktop GUI app | ❌ | ✅ (Electron) | ❌ | ❌ |
| REST API server | ❌ | ✅ (goosed) | ❌ | ❌ |
| Web interface | ❌ | ✅ | ❌ | ❌ |
| IDE integration | ❌ | ✅ (VS Code) | ✅ | ❌ |
| Voice dictation | ❌ | ✅ (Whisper) | ❌ | ❌ |
| Auto-visualization | ❌ | ✅ (Chart.js, D3) | ❌ | ❌ |
| Browser automation | ❌ | ❌ | ❌ | ✅ |
| Desktop automation | ❌ | ✅ (ComputerController) | ❌ | ✅ |

### Security & Enterprise

| Capability | Koda v0.1.1 | Goose | Claude Code | Code Puppy |
|------------|:-----------:|:-----:|:-----------:|:----------:|
| Tool confirmation | ✅ (approve/reject/feedback) | ✅ | ✅ | ✅ |
| Per-tool permissions | ❌ | ✅ (always/ask/never) | ✅ | ❌ |
| Prompt injection detection | ❌ | ✅ (pattern + ML) | ❌ | ❌ |
| Container sandbox | ❌ | ✅ (Docker) | ❌ | ❌ |
| OpenTelemetry (OTLP) | ❌ | ✅ | ❌ | ❌ |
| Scheduled tasks (cron) | ❌ | ✅ | ❌ | ❌ |
| Lead-worker multi-model | ❌ | ✅ | ❌ | ❌ |

### Code Analysis & Documents

| Capability | Koda v0.1.1 | Goose | Claude Code | Code Puppy |
|------------|:-----------:|:-----:|:-----------:|:----------:|
| Text search (grep/glob) | ✅ | ✅ | ✅ | ✅ |
| Tree-sitter AST analysis | ❌ | ✅ (11 languages) | ❌ | ❌ |
| Call graph generation | ❌ | ✅ | ❌ | ❌ |
| PDF/DOCX/XLSX reading | ❌ | ✅ | ❌ | ❌ |
| Screenshot capture | ❌ | ✅ (macOS) | ❌ | ❌ |
