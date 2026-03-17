# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a high-performance AI coding agent built in Rust (edition 2024). Four-crate workspace:
- `koda-core` (library) — pure engine with zero terminal deps
- `koda-cli` (binary `koda`) — CLI frontend with ratatui TUI
- `koda-ast` (binary `koda-ast`) — tree-sitter AST analysis (library + standalone MCP server)
- `koda-email` (binary `koda-email`) — email via IMAP/SMTP (library + standalone MCP server)

See [DESIGN.md](DESIGN.md) for architectural decisions. See [#70](https://github.com/lijunzh/koda/issues/70) for the TUI design.

### Current Architecture

Simple inference loop: stream LLM response → execute tool calls → repeat.
No phases, no tiers — the model drives execution directly.

- **Context from API** (`providers/`): queries actual context window from provider
  - Fallback: `model_context.rs` lookup table
- **Rate limit retry**: exponential backoff (2/4/8/16/32s) for 429 errors
- **Built-in agents**: default (others via user-created agent configs)
- **Git checkpointing** (`git.rs`): auto-snapshot before each turn

Approval is per-tool. Two modes (Auto/Confirm) control
how mutations are gated:

- **ToolEffect** (`approval.rs`): ReadOnly / LocalMutation / Destructive / RemoteAction
  - Auto: local mutations auto-approved, destructive need confirmation
  - Confirm: every non-read action needs confirmation
- **Hardcoded floors**: destructive ops and outside-project writes always need
  confirmation regardless of mode
- **Folder scoping** (`approval.rs`, `bash_safety.rs`):
  - `is_outside_project()`: checks file tool paths against project_root
  - `lint_bash_paths()`: heuristic bash command analysis for cd/path escapes
  - Startup warning when project_root == $HOME

## Documentation Rules

**When to update docs with a PR:**
- User-facing feature added/changed → update root README + relevant crate README
- Tool added/changed in koda-ast/koda-email → update the crate README's tool/protocol section
- Architecture or design decision → add numbered entry to DESIGN.md with rationale
- New crate → must ship with a README.md (required for crates.io)
- Keep feature coverage symmetric — if AST and email have equivalent capabilities, they get equivalent documentation
- Internal refactors don't require doc updates unless they change crate boundaries or public APIs

**On release:**
- Move CHANGELOG.md `[Unreleased]` to versioned section
- Bump version in all 4 crate Cargo.toml files (koda-core, koda-cli, koda-ast, koda-email)
- Verify README quick-start examples still work
- Check that CHANGELOG entries match what's documented in README/DESIGN.md

## Build & Development Commands

```bash
cargo build                              # Debug build (all crates)
cargo build --release -p koda-cli        # Release build (CLI only)
cargo test --workspace --features koda-core/test-support  # Run all tests (incl. E2E)
cargo test -p koda-core --features test-support          # Engine tests only
cargo test -p koda-cli                                   # CLI tests only
cargo test -p koda-ast                                   # AST library + server tests
cargo test -p koda-email                                 # Email library + server tests
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
│   │   ├── agent.rs        # KodaAgent (shared config: tools, prompt)
│   │   ├── approval.rs     # Approval modes + tool confirmation gates
│   │   ├── bash_path_lint.rs # Heuristic path-escape detection for bash commands
│   │   ├── bash_safety.rs  # Bash command safety classification
│   │   ├── compact.rs      # Session compaction (summarize old messages)
│   │   ├── config.rs       # Agent/provider config + provider metadata
│   │   ├── context.rs      # Context window token tracking
│   │   ├── db.rs           # SQLite persistence (WAL mode, parameterized queries)
│   │   ├── file_tracker.rs # File lifecycle tracking (create/edit/delete ownership)
│   │   ├── git.rs          # Git checkpointing + rollback
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── inference_helpers.rs # Token estimation, message assembly, overflow detection
│   │   ├── keystore.rs     # Secure API key storage (~/.config/koda/keys.toml, 0600)
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── memory.rs       # Semantic memory (global + project tiers → system prompt)
│   │   ├── model_context.rs# Model → context window size lookup table (fallback)
│   │   ├── output_caps.rs  # Output cap scaling based on context window size
│   │   ├── persistence.rs  # Persistence trait — the database contract
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── progress.rs     # Progress reporting helpers for long operations
│   │   ├── prompt.rs       # System prompt construction
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── settings.rs     # Runtime settings (approval mode, etc.)
│   │   ├── skills.rs       # Skill discovery and activation
│   │   ├── sub_agent_cache.rs # Sub-agent provider/model config cache
│   │   ├── tool_dispatch.rs# Tool dispatch — routes tool calls to the registry
│   │   ├── truncate.rs     # Token-safe output truncation
│   │   ├── undo.rs         # Undo stack for file mutations
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   ├── providers/      # LLM providers
│   │   │   ├── mod.rs      # LlmProvider trait, ChatMessage, HTTP client builder
│   │   │   ├── anthropic.rs# Anthropic Claude (prompt caching, extended thinking)
│   │   │   ├── gemini.rs   # Google Gemini (context caching, key-in-URL)
│   │   │   ├── openai_compat.rs # OpenAI-compatible (GPT, DeepSeek, local models)
│   │   │   ├── mock.rs     # Mock provider for tests
│   │   │   ├── stream_collector.rs # ChunkParser trait + shared SSE stream collector
│   │   │   └── stream_tag_filter.rs # Strip XML tool tags from streaming text
│   │   └── tools/          # Built-in tools
│   │       ├── mod.rs      # ToolRegistry, ToolEffect, tool definitions
│   │       ├── agent.rs    # Sub-agent invocation tool
│   │       ├── file_tools.rs # Read, Write, Edit, Delete
│   │       ├── glob_tool.rs# Glob file search
│   │       ├── grep.rs     # Ripgrep-based text search
│   │       ├── memory.rs   # MemoryRead, MemoryWrite
│   │       ├── recall.rs   # RecallContext (session history)
│   │       ├── shell.rs    # Bash command execution
│   │       ├── skill_tools.rs # ListSkills, ActivateSkill
│   │       └── web_fetch.rs# WebFetch (URL content retrieval)
│   └── tests/              # Engine integration tests
├── koda-cli/               # CLI binary
│   ├── src/
│   │   ├── main.rs         # CLI entry point (clap)
│   │   ├── lib.rs          # Crate root (exports acp_adapter)
│   │   ├── tui_app.rs      # Main TUI event loop (ratatui Viewport::Fullscreen)
│   │   ├── tui_context.rs  # TUI shared mutable state struct
│   │   ├── tui_handlers_inference.rs # Inference event handling (extracted from context)
│   │   ├── tui_render.rs   # EngineEvent → ratatui Line/Span rendering
│   │   ├── tui_commands.rs # Slash command dispatch (/help, /model, /sessions, etc.)
│   │   ├── tui_wizards.rs  # Interactive wizards (/provider, /compact, /agent)
│   │   ├── tui_output.rs   # Output bridge: scroll_buffer push helpers
│   │   ├── tui_viewport.rs # Fullscreen viewport layout + rendering
│   │   ├── tui_types.rs    # MenuContent, PromptMode, TuiState, shared TUI types
│   │   ├── scroll_buffer.rs# App-managed scrollback buffer with visual-line scroll
│   │   ├── mouse_select.rs # Mouse text selection + clipboard copy
│   │   ├── ansi_parse.rs   # ANSI escape code → ratatui Span conversion
│   │   ├── history_render.rs # Session history → scroll_buffer replay
│   │   ├── md_render.rs    # Streaming markdown → ratatui renderer
│   │   ├── completer.rs    # Tab completion (/commands, @files, /model names)
│   │   ├── diff_render.rs  # Diff preview → ratatui renderer (syntax highlighted)
│   │   ├── highlight.rs    # Syntax highlighting via syntect
│   │   ├── startup.rs      # Startup banner rendering
│   │   ├── repl.rs         # Slash command parsing + provider/model lists
│   │   ├── input.rs        # @file reference processing + image loading
│   │   ├── headless.rs     # Single-prompt headless mode
│   │   ├── sink.rs         # CliSink (channel forwarding for TUI)
│   │   ├── server.rs       # ACP server over stdio JSON-RPC
│   │   ├── acp_adapter.rs  # ACP protocol adapter
│   │   ├── onboarding.rs   # First-run wizard (provider + API key setup)
│   │   └── tool_history.rs # Tool output history for /expand
│   │   └── widgets/        # TUI widgets
│   │       ├── mod.rs      # Widget module root
│   │       ├── dropdown.rs # Generic dropdown widget
│   │       ├── file_menu.rs# @file autocomplete menu
│   │       ├── model_menu.rs # Model picker menu
│   │       ├── provider_menu.rs # Provider picker menu
│   │       ├── session_menu.rs # Session picker menu
│   │       ├── slash_menu.rs# Slash command dropdown (see DESIGN.md §14)
│   │       └── status_bar.rs# Model, mode, context meter, elapsed time
│   └── tests/              # CLI integration tests
├── koda-ast/               # Tree-sitter AST analysis (library + standalone MCP server)
│   ├── src/
│   │   ├── lib.rs          # Library entry point
│   │   ├── main.rs         # Standalone MCP server (rmcp, stdio transport)
│   │   └── ast.rs          # Tree-sitter analysis (Rust, Python, JS, TS)
│   └── tests/              # Integration tests
├── koda-email/             # Email via IMAP/SMTP (library + standalone MCP server)
│   ├── src/
│   │   ├── lib.rs          # Library entry point
│   │   ├── main.rs         # Standalone MCP server (rmcp, stdio transport)
│   │   ├── config.rs       # Credential loading from KODA_EMAIL_* env vars
│   │   ├── imap_client.rs  # IMAP read/search (sync imap crate + spawn_blocking)
│   │   └── smtp_client.rs  # SMTP sending via lettre
│   └── tests/              # Integration tests (two-layer)
└── DESIGN.md               # Architecture decisions
```

### Core Event Loop

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` → `inference_loop()` (streaming LLM + tools)

The TUI uses `ratatui::Viewport::Fullscreen` (alternate screen buffer) with
app-managed scrollback. All output flows through a single rendering path:
`EngineEvent` → `tui_render.rs` → `ScrollBuffer` → `draw_viewport()`.

**Viewport layout** (see DESIGN.md §14):
```
[history panel]                ← ScrollBuffer: scrollable, mouse-selectable
─── 🐻 ─                        ← separator
⚡> input                      ← tui-textarea, sized to content
──────────────────────────────
model │ auto │ ████ 42% │ 🐻 12s  ← status bar
[menu_area]                    ← dropdown / approval / wizard (Min(0))
```

All interactive UI renders in `menu_area`. The history panel fills the
remaining vertical space. Mouse scroll works during inference. Native
clipboard (`arboard`) handles copy since alternate screen disables
terminal-native mouse selection.

See DESIGN.md §14 for the interaction system design and competitive analysis.

The engine communicates through `EngineEvent` (output) and `EngineCommand` (input) enums.
Approval flows through async channels: engine emits `ApprovalRequest`, client sends `ApprovalResponse`.

### Key Types

- **`KodaAgent`** — Shared resources (tools, system prompt). `Arc`-shareable.
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
- Single rendering path in koda-cli:
  - All output → `ScrollBuffer` → `draw_viewport()` (fullscreen alternate screen)
  - No `insert_before()`, no dual-path rendering
- Engine → client: `EngineSink::emit(EngineEvent)`
- Client → engine: `mpsc::Receiver<EngineCommand>`
- Cancellation: `tokio_util::sync::CancellationToken`
- Cohesion over line count: don't split a file just because it's long.
  Split when pieces have genuinely independent responsibilities. An
  800-line file with one cohesive flow beats two 400-line files that
  require cross-file context-switching. Test: if you must read file B
  every time you read file A, they should be the same file.

## Test Structure

### Quick reference

```bash
# CI suite (all tests including E2E with mock provider)
cargo test --workspace --features koda-core/test-support

# Individual crate tests
cargo test -p koda-core --features test-support   # Engine (unit + E2E)
cargo test -p koda-cli                             # CLI (unit + integration)
cargo test -p koda-ast                             # AST (unit + protocol)
cargo test -p koda-email                           # Email (unit + protocol)

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

#### koda-ast

**Unit tests** — co-located in `src/`:
- AST parsing, call graph extraction, language detection

**Integration tests** — spawn binary, send JSON-RPC over stdio:
- `tests/mcp_integration_test.rs`:
  - `test_mcp_initialize` — server starts, reports capabilities
  - `test_mcp_tools_list` — AstAnalysis tool present
  - `test_mcp_analyze_file` — analyzes a real Rust file
  - `test_mcp_file_not_found` — graceful error for missing file

#### koda-email

Two-layer test strategy:

**Layer 1 — Always run (no external deps):**

Unit tests + protocol tests. These verify the server starts,
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

### Integration test pattern (MCP servers)

Both `koda-ast` and `koda-email` binaries use the same integration test pattern:
1. Spawn the server binary as a child process
2. Pipe JSON-RPC messages over stdin/stdout
3. Send `initialize` + `notifications/initialized` handshake
4. Call `tools/list` or `tools/call` and assert on responses
5. Kill the child process

This pattern should be reused for any future standalone servers added to the workspace.
See `koda-ast/tests/mcp_integration_test.rs` for the reference implementation.

### Adding a new first-party capability checklist

For capabilities that ship in the koda workspace (same release cycle):

1. Create `koda-<name>/` workspace member with `src/lib.rs` + `src/main.rs`, `Cargo.toml`
2. Add to workspace `members` in root `Cargo.toml`
3. Export `pub fn tool_definitions()` from the library crate
4. Add `koda-<name>` as a dependency of `koda-core` in `Cargo.toml`
5. Register tools via `tool_definitions()` in `ToolRegistry::new()`
6. Add match arms in `ToolRegistry::execute()` for each tool
7. Add `--version` flag to `main.rs` (standalone server wrapper)
8. Write integration tests in `tests/mcp_integration_test.rs`
9. Update `release.yml`: version verify, build, package, publish, Homebrew
10. Sync version with workspace (currently 0.1.10)
11. Update this file (CLAUDE.md)

