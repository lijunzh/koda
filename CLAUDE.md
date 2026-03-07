# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a high-performance AI coding agent built in Rust (edition 2024). Four-crate workspace:
- `koda-core` (library) — pure engine with zero terminal deps
- `koda-cli` (binary `koda`) — CLI frontend with ratatui TUI
- `koda-ast` (binary `koda-ast`) — MCP server for tree-sitter AST analysis
- `koda-email` (binary `koda-email`) — MCP server for email via IMAP/SMTP

See [DESIGN.md](DESIGN.md) for architectural decisions. See [#70](https://github.com/lijunzh/koda/issues/70) for the TUI design.

## Build & Development Commands

```bash
cargo build                              # Debug build (all crates)
cargo build --release -p koda-cli        # Release build (CLI only)
cargo test --workspace --features koda-core/test-support  # Run all tests (incl. E2E)
cargo test -p koda-core --features test-support          # Engine tests only
cargo test -p koda-cli                                   # CLI tests only
cargo test -p koda-ast                                   # AST MCP server tests
cargo test -p koda-email                                 # Email MCP server tests
cargo test -p koda-core --test perf_test                 # Run a specific test file
cargo fmt --all                          # Format all crates
cargo fmt --all --check                  # Check formatting (CI enforced)
cargo clippy --workspace -- -D warnings  # Lint (CI enforced)
cargo doc --workspace --no-deps          # Build docs
```

## Architecture

### Workspace

```
koda/
├── Cargo.toml              # Workspace root
├── koda-core/              # Engine library (zero terminal deps)
│   ├── src/
│   │   ├── lib.rs          # Crate root
│   │   ├── agent.rs        # KodaAgent (shared config: tools, prompt, MCP)
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── inference_helpers.rs # Token estimation, message assembly, overflow detection
│   │   ├── compact.rs      # Session compaction (summarize old messages)
│   │   ├── approval.rs     # Approval modes + bash command safety classification
│   │   ├── context.rs      # Context window token tracking
│   │   ├── keystore.rs     # Secure API key storage (~/.config/koda/keys.toml, 0600)
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── memory.rs       # Semantic memory (global + project tiers → system prompt)
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   ├── providers/      # LLM providers (Anthropic, Gemini, OpenAI-compat, mock)
│   │   ├── tools/          # Built-in tools (Bash, Read, Write, Edit, etc.)
│   │   ├── mcp/            # MCP client (registry, config, capability_registry)
│   │   ├── db.rs           # SQLite persistence (WAL mode, parameterized queries)
│   │   └── config.rs       # Agent/provider config
│   └── tests/              # Engine integration tests
├── koda-cli/               # CLI binary
│   ├── src/
│   │   ├── main.rs         # CLI entry point (clap)
│   │   ├── tui_app.rs      # Main TUI event loop (ratatui Viewport::Inline)
│   │   ├── tui_render.rs   # EngineEvent → ratatui Line/Span rendering
│   │   ├── tui_commands.rs # Slash command dispatch (/help, /model, /sessions, etc.)
│   │   ├── tui_wizards.rs  # Interactive wizards (/provider, /compact, /mcp, /agent)
│   │   ├── tui_output.rs   # Output bridge: emit_line (ratatui) + write_line (crossterm)
│   │   ├── md_render.rs    # Streaming markdown → ratatui renderer
│   │   ├── completer.rs    # Tab completion (/commands, @files, /model names)
│   │   ├── diff_render.rs  # Diff preview → ratatui renderer (syntax highlighted)
│   │   ├── highlight.rs    # Syntax highlighting via syntect
│   │   ├── select_menu.rs  # Arrow-key selection menus (standalone + inline)
│   │   ├── commands.rs     # Provider factory (create_provider)
│   │   ├── repl.rs         # Slash command parsing + provider/model lists
│   │   ├── input.rs        # @file reference processing + image loading
│   │   ├── headless.rs     # Single-prompt headless mode
│   │   ├── headless_sink.rs# HeadlessSink (println-based, auto-approve)
│   │   ├── sink.rs         # CliSink (channel forwarding for TUI)
│   │   ├── server.rs       # ACP server over stdio JSON-RPC
│   │   ├── acp_adapter.rs  # ACP protocol adapter
│   │   ├── onboarding.rs   # First-run wizard (provider + API key setup)
│   │   ├── interrupt.rs    # Ctrl+C double-tap graceful cancellation
│   │   ├── tool_history.rs # Tool output history for /expand
│   │   ├── lib.rs          # Crate root (exports acp_adapter)
│   │   └── widgets/        # TUI widgets
│   │       ├── approval.rs # Inline approval prompt (approve/reject/feedback)
│   │       ├── status_bar.rs# Model, mode, context meter, elapsed time
│   │       └── text_input.rs# Inline text input (masked for API keys)
│   └── tests/              # CLI integration tests
├── koda-ast/               # MCP server: tree-sitter AST analysis
│   ├── src/
│   │   ├── main.rs         # MCP server (rmcp, stdio transport)
│   │   └── ast.rs          # Tree-sitter analysis (Rust, Python, JS, TS)
│   └── tests/              # MCP integration tests
├── koda-email/             # MCP server: email via IMAP/SMTP
│   ├── src/
│   │   ├── main.rs         # MCP server (rmcp, stdio transport)
│   │   ├── config.rs       # Credential loading from KODA_EMAIL_* env vars
│   │   ├── imap_client.rs  # IMAP read/search (sync imap crate + spawn_blocking)
│   │   └── smtp_client.rs  # SMTP sending via lettre
│   └── tests/              # MCP integration tests (two-layer)
└── DESIGN.md               # Architecture decisions
```

### Core Event Loop

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` → `inference_loop()` (streaming LLM + tools)

The TUI uses `ratatui::Viewport::Inline` for a persistent input bar + status bar at the bottom.
Engine output is rendered above the viewport via `insert_before()`. Slash commands use
crossterm direct writes (`write_line()`). These two rendering paths must never be mixed
within a single operation.

The engine communicates through `EngineEvent` (output) and `EngineCommand` (input) enums.
Approval flows through async channels: engine emits `ApprovalRequest`, client sends `ApprovalResponse`.

### Key Types

- **`KodaAgent`** — Shared resources (tools, system prompt, MCP). `Arc`-shareable.
- **`KodaSession`** — Per-conversation state (DB, provider, settings). Has `run_turn()`.
- **`EngineSink`** — Trait with single method: `fn emit(&self, event: EngineEvent)`.
- **`CliSink`** — Channel-forwarding sink. Sends events to the TUI event loop via `UiEvent`.

### Provider System (`koda-core/src/providers/`)

All providers implement `LlmProvider` trait (`chat_stream` returning `Receiver<StreamChunk>`).

### Tool System (`koda-core/src/tools/`)

Tools use PascalCase names. `mod.rs` has the registry, dispatcher, and `safe_resolve_path()`.

## Conventions

- Error handling: `anyhow::Result<T>` with `.context()`
- All I/O is async (`tokio`)
- Tool names: PascalCase; module names: snake_case
- `koda-core` has zero terminal deps (no crossterm, no ratatui)
- Two rendering paths in koda-cli (never mix within one operation):
  - `emit_line()` / `emit_above()` — ratatui `insert_before()` for engine output
  - `write_line()` — crossterm direct writes for slash commands
- Engine → client: `EngineSink::emit(EngineEvent)`
- Client → engine: `mpsc::Receiver<EngineCommand>`
- Cancellation: `tokio_util::sync::CancellationToken`

## Test Structure

### Quick reference

```bash
# CI suite (all tests including E2E with mock provider)
cargo test --workspace --features koda-core/test-support

# Individual crate tests
cargo test -p koda-core --features test-support   # Engine (unit + E2E)
cargo test -p koda-cli                             # CLI (unit + integration)
cargo test -p koda-ast                             # AST MCP (unit + MCP protocol)
cargo test -p koda-email                           # Email MCP (unit + MCP protocol)

# Live/opt-in tests (require external services)
KODA_TEST_LMSTUDIO=1 cargo test -p koda-cli --test smoke_test -- --ignored
cargo test -p koda-email -- --ignored              # Requires KODA_EMAIL_* env vars
```

The `test-support` feature gates `MockProvider` and `TestSink` — excluded
from production builds to keep `koda-core`'s public API clean.

### Test tiers by crate

#### koda-core

**Unit tests** — co-located in `src/` modules, no feature flag:
- Context tracking, loop detection, config defaults, bash safety, etc.

**E2E tests** (mock provider, CI) — require `test-support` feature:
- `tests/e2e_test.rs` — full inference loop with real tools in sandboxed temp dirs
- `tests/cancel_test.rs` — Ctrl+C interruption during inference

**Integration tests** — no feature flag:
- `tests/file_tools_test.rs` — path safety, file CRUD
- `tests/new_tools_test.rs` — glob, tool naming
- `tests/perf_test.rs` — DB, grep, markdown throughput
- `tests/capabilities_test.rs` — capabilities.md freshness

#### koda-cli

**Unit tests** — co-located in `src/`:
- Markdown rendering, REPL parsing, highlighting

**Integration tests** — no feature flag:
- `tests/cli_test.rs` — binary subprocess invocation
- `tests/regression_test.rs` — REPL dispatch, input processing
- `tests/server_test.rs` — ACP server integration (JSON-RPC lifecycle)

**Live smoke tests** (`#[ignore]`, local only):
- `tests/smoke_test.rs` — headless prompt, tool use, session resume against LM Studio
- Gated by `KODA_TEST_LMSTUDIO=1` env var; never runs in CI

#### koda-ast (MCP server)

**Unit tests** — co-located in `src/`:
- AST parsing, call graph extraction, language detection

**MCP integration tests** — spawn binary, send JSON-RPC over stdio:
- `tests/mcp_integration_test.rs`:
  - `test_mcp_initialize` — server starts, reports capabilities
  - `test_mcp_tools_list` — AstAnalysis tool present
  - `test_mcp_analyze_file` — analyzes a real Rust file
  - `test_mcp_file_not_found` — graceful error for missing file

#### koda-email (MCP server)

Two-layer test strategy:

**Layer 1 — Always run (no external deps):**

Unit tests + MCP protocol tests. These verify the server starts,
tools are registered, schemas are well-formed, and missing credentials
produce helpful setup instructions instead of crashes.

- `tests/mcp_integration_test.rs`:
  - `test_mcp_initialize` — server starts, reports name + capabilities
  - `test_mcp_tools_list` — all 3 tools present (EmailRead/Send/Search)
  - `test_tool_schemas_have_descriptions` — schemas well-formed
  - `test_email_read_without_credentials` — returns setup instructions
  - `test_email_send_without_credentials` — returns setup instructions
  - `test_email_search_without_credentials` — returns setup instructions
  - `test_version_flag` — `--version` prints correctly

**Layer 2 — Opt-in (`#[ignore]`, needs real IMAP/SMTP credentials):**

```bash
# Set credentials first:
export KODA_EMAIL_IMAP_HOST=imap.gmail.com
export KODA_EMAIL_USERNAME=you@gmail.com
export KODA_EMAIL_PASSWORD=your-app-password

# Run live tests:
cargo test -p koda-email -- --ignored
```

- `test_live_email_read` — fetches real emails via IMAP
- `test_live_email_search` — searches real mailbox

### MCP integration test pattern

Both `koda-ast` and `koda-email` use the same MCP integration test pattern:
1. Spawn the MCP server binary as a child process
2. Pipe JSON-RPC messages over stdin/stdout
3. Send `initialize` + `notifications/initialized` handshake
4. Call `tools/list` or `tools/call` and assert on responses
5. Kill the child process

This pattern should be reused for any future MCP servers added to the workspace.
See `koda-ast/tests/mcp_integration_test.rs` for the reference implementation.

### Adding a new MCP server checklist

1. Create `koda-<name>/` workspace member with `src/main.rs`, `Cargo.toml`
2. Add to workspace `members` in root `Cargo.toml`
3. Register tools in `koda-core/src/mcp/capability_registry.rs`
4. Add `--version` flag to `main()`
5. Write MCP integration tests in `tests/mcp_integration_test.rs`
6. Update `release.yml`: version verify, build, package, publish, Homebrew
7. Sync version with workspace (currently 0.1.2)
8. Update this file (claude.md)
