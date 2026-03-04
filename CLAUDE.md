# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a high-performance AI coding agent built in Rust (edition 2024). Two-crate workspace:
- `koda-core` (library) — pure engine with zero terminal deps
- `koda-cli` (binary `koda`) — CLI frontend

See [DESIGN.md](DESIGN.md) for architectural decisions. See [#21](https://github.com/lijunzh/koda/issues/21) for the v0.1.0 release plan.

## Build & Development Commands

```bash
cargo build                              # Debug build
cargo build --release -p koda-cli        # Release build
cargo test --workspace                   # Run all 347 tests
cargo test -p koda-core                  # Engine tests only
cargo test -p koda-cli                   # CLI tests only
cargo test -p koda-core --test perf_test # Run a specific test file
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
├── koda-core/              # Engine library
│   ├── src/
│   │   ├── lib.rs          # Crate root
│   │   ├── agent.rs        # KodaAgent (shared config: tools, prompt, MCP)
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── approval.rs     # Approval modes + bash command safety classification
│   │   ├── context.rs      # Context window token tracking
│   │   ├── keystore.rs     # Secure API key storage (~/.config/koda/keys.toml, 0600)
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── memory.rs       # Semantic memory (global + project tiers → system prompt)
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   ├── providers/      # LLM providers (Anthropic, Gemini, OpenAI-compat)
│   │   ├── tools/          # Built-in tools (Bash, Read, Write, Edit, etc.)
│   │   ├── mcp/            # MCP client
│   │   ├── db.rs           # SQLite persistence
│   │   └── config.rs       # Agent/provider config
│   └── tests/              # Engine integration tests
├── koda-cli/               # CLI binary
│   ├── src/
│   │   ├── main.rs         # CLI entry point (clap)
│   │   ├── app.rs          # Application entry points (REPL + headless dispatch)
│   │   ├── headless.rs     # Single-prompt headless mode
│   │   ├── repl.rs         # Slash command handling (/model, /provider, /help, /quit)
│   │   ├── commands.rs     # /compact, /mcp, /provider, /trust handlers
│   │   ├── confirm.rs      # User confirmation UI for dangerous operations
│   │   ├── input.rs        # rustyline Helper: slash-command + @file completions
│   │   ├── interrupt.rs    # Ctrl+C double-tap graceful cancellation
│   │   ├── onboarding.rs   # First-run wizard (provider + API key setup)
│   │   ├── tui.rs          # Arrow-key interactive selection menus
│   │   ├── sink.rs         # CliSink (EngineEvent → terminal rendering)
│   │   ├── display.rs      # Terminal output formatting
│   │   ├── highlight.rs    # Syntax highlighting via syntect
│   │   └── markdown.rs     # Streaming markdown renderer
│   └── tests/              # CLI integration tests
└── DESIGN.md               # Architecture decisions
```

### Core Event Loop

`main.rs` → `app.rs` (REPL) → `KodaSession::run_turn()` → `inference_loop()` (streaming LLM + tools)

The engine communicates through `EngineEvent` (output) and `EngineCommand` (input) enums.
Approval flows through async channels: engine emits `ApprovalRequest`, client sends `ApprovalResponse`.

### Key Types

- **`KodaAgent`** — Shared resources (tools, system prompt, MCP). `Arc`-shareable.
- **`KodaSession`** — Per-conversation state (DB, provider, settings). Has `run_turn()`.
- **`EngineSink`** — Trait with single method: `fn emit(&self, event: EngineEvent)`.
- **`CliSink`** — CLI implementation. Renders events to terminal + sends approval responses via channel.

### Provider System (`koda-core/src/providers/`)

All providers implement `LlmProvider` trait (`chat_stream` returning `Receiver<StreamChunk>`).

### Tool System (`koda-core/src/tools/`)

Tools use PascalCase names. `mod.rs` has the registry, dispatcher, and `safe_resolve_path()`.

## Conventions

- Error handling: `anyhow::Result<T>` with `.context()`
- All I/O is async (`tokio`)
- Tool names: PascalCase; module names: snake_case
- `koda-core` has zero terminal deps (no crossterm, no rustyline)
- Engine → client: `EngineSink::emit(EngineEvent)`
- Client → engine: `mpsc::Receiver<EngineCommand>`
- Cancellation: `tokio_util::sync::CancellationToken`

## Test Structure

**koda-core** (unit + integration):
- Unit tests co-located in `src/` modules
- `tests/file_tools_test.rs` — path safety, file CRUD
- `tests/new_tools_test.rs` — glob, tool naming
- `tests/perf_test.rs` — DB, grep, markdown throughput
- `tests/capabilities_test.rs` — capabilities.md freshness

**koda-cli** (unit + integration):
- Unit tests in `src/` modules
- `tests/cli_test.rs` — binary subprocess invocation
- `tests/regression_test.rs` — REPL dispatch, input processing
