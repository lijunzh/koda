# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a high-performance AI coding agent built in Rust (edition 2024). Single compiled binary, multi-provider LLM support, zero runtime dependencies. Published as `koda-agent` on crates.io, binary name is `koda`.

## Build & Development Commands

```bash
cargo build                          # Debug build
cargo build --release                # Release build (stripped, LTO)
cargo run                            # Run locally
cargo test                           # Run all tests (~288 tests)
cargo test --test cli_test           # Run a specific test file
cargo test test_cli_version -- --exact  # Run a single test by name
cargo test -- --nocapture            # Show stdout/stderr in tests
cargo fmt                            # Format code
cargo fmt --check                    # Check formatting (CI enforced)
cargo clippy --all-targets -- -D warnings  # Lint (CI enforced, warnings are errors)
cargo doc --no-deps                  # Build docs (RUSTDOCFLAGS=-Dwarnings in CI)
```

CI runs: check, fmt, clippy, test (Linux/macOS/Windows), doc, and security audit.

## Architecture

### Core Event Loop

`main.rs` → CLI parsing (clap) → `app.rs` (main event loop) → `inference.rs` (streaming LLM calls + tool execution loop)

The per-turn cycle: assemble system prompt (with semantic memory) → load conversation history from SQLite → stream LLM response via SSE → execute any tool calls → feed results back until LLM produces text without tool calls.

### Key Modules

- **`app.rs`** — Main event loop, provider creation, session management
- **`inference.rs`** — Streaming inference, tool dispatch, auto-compact logic
- **`config.rs`** — Agent/provider config loading with CLI override cascade
- **`db.rs`** — SQLite layer (WAL mode, `sqlx`), sessions + messages tables
- **`repl.rs`** — Slash commands, banner, prompt handling
- **`input.rs`** — Tab completions, hints, `@file` reference injection
- **`memory.rs`** — Loads MEMORY.md / CLAUDE.md / AGENTS.md into system prompt
- **`markdown.rs`** — Streaming token-by-token markdown renderer with syntect highlighting

### Provider System (`src/providers/`)

All providers implement the `LlmProvider` trait (`chat_stream` returning `Receiver<StreamChunk>`). StreamChunk variants: `TextDelta`, `ToolCalls`, `Done(TokenUsage)`.

- `anthropic.rs` — Claude (with prompt caching)
- `openai_compat.rs` — OpenAI, LM Studio, Groq, Grok
- `gemini.rs` — Native Google Gemini API

### Tool System (`src/tools/`)

Tools use PascalCase names (e.g., `Read`, `Write`, `Grep`, `WebFetch`). Each module exposes `definitions() -> Vec<ToolDefinition>` and execution functions. `mod.rs` contains the tool registry, dispatcher, and `safe_resolve_path()` for path traversal protection.

Destructive tools (`Write`, `Edit`, `Delete`, `Bash`) require user confirmation via `confirm.rs`.

### Agent System

Agent configs are JSON files in `agents/`. Five built-in agents are embedded at compile time. Resolution order: project `agents/` → user `~/.config/koda/agents/` → built-in. Sub-agents run in isolated inference loops sharing the same tool registry and provider.

### Database

SQLite with WAL mode at `.koda.db`. Two tables: `sessions` and `messages`. Messages store role, content, tool_calls (JSON), and token usage. Sliding-window context loading prevents unbounded growth.

## Conventions

- Error handling: `anyhow::Result<T>` throughout, with `.context()` for meaningful errors
- All I/O is async (`tokio`), tool execution uses `futures::join_all()` for parallel safe tools
- Tool names are PascalCase; module names are snake_case
- Agent configs are JSON; API keys stored in `~/.config/koda/keys.toml`
- Environment overrides: `KODA_PROVIDER`, `KODA_MODEL`, `KODA_BASE_URL`

## Test Structure

- `tests/cli_test.rs` — Binary invocation (--version, --help, headless mode)
- `tests/file_tools_test.rs` — File CRUD integration tests
- `tests/new_tools_test.rs` — Glob, WebFetch, Todo, Constructor tests
- `tests/perf_test.rs` — Performance benchmarks (DB, grep, markdown, SSE parsing)
- `tests/regression_test.rs` — Command dispatch, completions, naming conventions
- Unit tests co-located in `src/` modules
