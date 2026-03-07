# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a high-performance AI coding agent built in Rust (edition 2024). Two-crate workspace:
- `koda-core` (library) — pure engine with zero terminal deps
- `koda-cli` (binary `koda`) — CLI frontend with ratatui TUI

See [DESIGN.md](DESIGN.md) for architectural decisions. See [#70](https://github.com/lijunzh/koda/issues/70) for the TUI design.

## Build & Development Commands

```bash
cargo build                              # Debug build
cargo build --release -p koda-cli        # Release build
cargo test --workspace --features koda-core/test-support  # Run all tests (incl. E2E)
cargo test -p koda-core --features test-support          # Engine tests only
cargo test -p koda-cli                                   # CLI tests only
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
│   │   ├── mcp/            # MCP client (registry, config, stdio transport)
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

### Running tests

```bash
# CI suite (all tests including E2E with mock provider)
cargo test --workspace --features koda-core/test-support

# Live smoke tests (requires LM Studio running locally)
KODA_TEST_LMSTUDIO=1 cargo test -p koda-cli --test smoke_test -- --ignored
```

The `test-support` feature gates `MockProvider` and `TestSink` — they are excluded
from production builds to keep `koda-core`'s public API clean.

### Test tiers

**Unit tests** — co-located in `src/` modules, no feature flag needed:
```bash
cargo test -p koda-core   # runs unit tests only (no E2E)
```

**E2E tests** (mock provider, CI) — require `test-support` feature:
- `koda-core/tests/e2e_test.rs` — full inference loop with real tools in sandboxed temp dirs
- `koda-core/tests/cancel_test.rs` — Ctrl+C interruption during inference

**Integration tests** — no feature flag needed:
- `koda-core/tests/file_tools_test.rs` — path safety, file CRUD
- `koda-core/tests/new_tools_test.rs` — glob, tool naming
- `koda-core/tests/perf_test.rs` — DB, grep, markdown throughput
- `koda-core/tests/capabilities_test.rs` — capabilities.md freshness

**CLI tests** — no feature flag needed:
- `koda-cli/tests/cli_test.rs` — binary subprocess invocation
- `koda-cli/tests/regression_test.rs` — REPL dispatch, input processing
- `koda-cli/tests/server_test.rs` — ACP server integration (JSON-RPC lifecycle)

**Live smoke tests** (`#[ignore]`, local only):
- `koda-cli/tests/smoke_test.rs` — headless prompt, tool use, session resume against LM Studio
- Gated by `KODA_TEST_LMSTUDIO=1` env var; never runs in CI
