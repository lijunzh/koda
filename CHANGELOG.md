# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

> **Lineage:** This project continues from [`koda-agent`](https://github.com/lijunzh/koda-agent) (archived at v0.1.5).
> Versions v0.1.0–v0.1.5 of `koda-agent` are documented in that repository's CHANGELOG.

## [Unreleased]

## [0.1.3] - 2026-03-06

### Added
- **Model-adaptive architecture** — `ModelTier` enum (Strong/Standard/Lite) auto-detected from model name + provider
  - Strong: minimal prompts, lazy tool loading, parallel execution, 90% auto-compact
  - Standard: full prompts, all tools, 80% auto-compact (backward compatible)
  - Lite: verbose prompts, sequential execution, 70% auto-compact, 50 iteration cap
  - CLI override: `--model-tier strong|standard|lite`
  - Agent config: `"model_tier": "strong"` in JSON
  - Displayed in status bar: `claude-sonnet-4-6 [Strong]`
- **Context window auto-detection** — maps model name to actual context size
  - Opus: 32K → 200K, Gemini 2.5: 32K → 1M, GPT-4o: 32K → 128K
  - Eliminates premature compaction (Opus was using 16% of available context)
- **Rate limit retry** — exponential backoff (2/4/8/16/32s) for 429 errors, up to 5 retries
- **DiscoverTools** tool — on-demand tool schema injection by category (agents, skills, web, memory, ast, email)
  - Strong tier loads 9 core tools + DiscoverTools (~850 tokens vs ~2000)
  - 57% reduction in per-turn tool overhead for Strong tier
- **RecallContext** tool — search or recall older conversation turns that scrolled out of the sliding window
- **Task phase state machine** — auto-detects Understanding → Planning → Executing → Verifying → Reporting
- **Intent classifier** — rule-based task classification with agent/skill suggestions (zero LLM cost)
  - "write tests" → testgen, "find all uses" → scout, "review" → review skill
- **Built-in scout agent** — read-only codebase explorer (Read, List, Grep, Glob), max 10 iterations
- **Built-in planner agent** — strategic task decomposition (read-only), max 5 iterations
- **Built-in verifier agent** — quality verification (Bash, Read, Grep), max 8 iterations
- **Sub-agent model routing** — sub-agents respect their own provider/model when explicitly set
- **Plan-before-execute** — system prompt instructs planning for >3-step tasks
- **Self-review instruction** — verify feasibility before executing multi-step plans
- **koda-email MCP server** — email read/send/search via IMAP/SMTP (any provider)

### Fixed
- **Thinking tokens in cost** — `estimate_turn_cost()` now includes thinking tokens at output rate. Opus with extended thinking budget no longer underreports cost by 2-3x.
- **Token estimation calibration** — chars/3.5 heuristic (was chars/4) for better accuracy with code
- **`__INVOKE_AGENT__` sentinel removed** — InvokeAgent handled at dispatch level, no more magic strings
- **Email tool normalizer mappings** — EmailRead/Send/Search properly normalized from lowercase

### Testing
- 489 tests across 4 crates (up from 284 in v0.1.2)
- New: model tier tests, context window tests, rate limit tests, DiscoverTools tests, RecallContext tests, task phase tests, intent classifier tests, email MCP integration tests

## [0.1.2] - 2026-03-06

### Added
- **Inline TUI** — ratatui `Viewport::Inline` with persistent input + status bar ([#70](https://github.com/lijunzh/koda/issues/70))
  - Type-ahead during inference (input queued while model runs)
  - Inline approval widget (arrow-key approve/reject/feedback)
  - Status bar: model name, approval mode, context meter (`████░░ 5%`), elapsed time
  - Dynamic viewport expansion: input area grows with multi-line text (2–10 rows)
  - Paste detection: multi-line paste enters text mode instead of submitting
- **Streaming markdown renderer** — headers, **bold**, *italic*, `code`, fenced blocks with syntax highlighting, lists, blockquotes, horizontal rules
- **Tab completion** — three modes:
  - Slash commands: `/d` + Tab → dropdown select (`/diff`, `/diff commit`, `/diff review`)
  - `@file` paths: `@src/m` + Tab → dropdown with filesystem walking (case-insensitive)
  - `/model` names: `/model gpt` + Tab → dropdown with substring matching
- **Compaction module** — `koda-core::compact` with pure logic, zero UI deps. Shared by TUI and headless modes
- **Alt+Enter** for multi-line input (Shift+Enter on terminals with kitty protocol)

### Fixed
- **TUI auto-compaction** — was calling `println!` inside raw mode, corrupting the viewport
- **API key echoing** — onboarding now uses `rpassword` for silent input
- **Path traversal in @file** — `@../../etc/passwd` now blocked by `safe_resolve_path()`
- **Select menu cleanup** — leftover menu items no longer linger after `/provider`, `/model`
- **Rendering path consistency** — all slash commands use crossterm; approval widget fixed
- **Event clone in hot path** — `TextDelta` events no longer cloned during streaming
- **Lock poisoning** — `runtime_env` recovers gracefully instead of panicking
- **Raw mode RAII guard** — `select_menu` restores terminal on panic

### Changed
- **Legacy cleanup** — deleted ~550 lines of dead code (`commands.rs`, old `handle_compact`, ANSI helpers)
- **DRY style helpers** — `ok_msg`/`err_msg`/`dim_msg`/`warn_msg` shared from `tui_output.rs`
- **Dropped rustyline** — replaced by `tui-textarea` widget

### Removed
- `app.rs` (864 lines) — legacy rustyline event loop
- `display.rs` (922 lines) — legacy terminal output formatting
- `markdown.rs` (564 lines) — legacy ANSI markdown renderer (replaced by `md_render.rs`)
- `confirm.rs` (104 lines) — legacy confirmation prompts

### Testing
- 284 tests across `koda-core` and `koda-cli`
- New: 12 compaction tests (7 unit + 2 E2E + skip/boundary), 12 markdown tests, 19 completer tests, 2 path traversal tests

## [0.1.1] - 2026-03-05

### Added
- **Async REPL event loop** — readline runs on a dedicated OS thread; inference, UI rendering, and approval prompts run concurrently via `tokio::select!`
- **Tool output expand/collapse** — `/expand N` reprints full output; `/verbose` toggles persistent expansion
- **TodoRead tool** — read and display task lists from the database
- **Todo list display** — active tasks shown after each turn with highlighting
- **Dev workflow guidance** — system prompt teaches best practices for development workflows
- **Pre-confirmation diff previews** — see exactly what Edit/Write/Delete will change before approving
- **Redundant diff skip** — suppress post-execution diff when preview was already shown
- **Persist provider/model** — last-used provider and model restored on startup
- **Diff background colors** — colored diff output with smarter shell error display
- **Interactive session resume** — `/sessions` shows an arrow-key picker to switch sessions mid-REPL
- **Session recovery** — orphaned tool calls from interrupted sessions are cleaned up on resume

### Fixed
- **Panic on multi-byte chars** — think_tag_filter no longer panics on emoji/CJK in thinking blocks
- **AstAnalysis approval** — now correctly classified as read-only (was requiring confirmation in Normal mode)
- **REPL survives inference errors** — API failures print an error and return to prompt instead of exiting
- **Improved TodoWrite prompts** — more reliable tool usage by small models

### Changed
- **rmcp** upgraded from 0.16 to 1.1

### Removed
- **Bottom bar / ANSI scroll regions** — reverted due to fundamental incompatibility with terminal scrollback. See [#57](https://github.com/lijunzh/koda/issues/57) for the TUI migration plan.

### Known Limitations
- **No type-ahead during inference** — input is not accepted while the model is running. Planned for v0.1.2 via a TUI framework migration ([#57](https://github.com/lijunzh/koda/issues/57)).

### Testing
- 372 tests across `koda-core` and `koda-cli`

## [0.1.0] - 2026-03-04

First release of `koda-core` and `koda-cli` as separate crates.

### Architecture
- **Workspace split**: `koda-agent` (single crate) → `koda-core` (library) + `koda-cli` (binary)
  - `koda-core`: pure engine with zero terminal dependencies
  - `koda-cli`: CLI frontend, produces the `koda` binary
  - `cargo install koda-cli` replaces `cargo install koda-agent`
- **Channel-based approval**: Async `EngineEvent::ApprovalRequest` / `EngineCommand::ApprovalResponse` over `tokio::mpsc` channels — transport-agnostic
- **CancellationToken**: Replaces global `AtomicBool` interrupt flag
- **KodaAgent**: Shared, immutable agent resources (tools, prompt, MCP registry). `Arc`-shareable
- **KodaSession**: Per-conversation state (DB, provider, settings, cancel token). `run_turn()` replaces 15-parameter `inference_loop()` call

### Added
- **ACP server** (`koda server --stdio`): JSON-RPC server over stdio implementing the Agent Client Protocol for editor integration (Zed, VS Code, etc.)
  - Full ACP lifecycle: Initialize → Authenticate → NewSession → Prompt (streaming) → Cancel
  - All 19 EngineEvent variants mapped to ACP protocol messages
  - Bidirectional approval flow over JSON-RPC

### Testing
- 360 tests across `koda-core` and `koda-cli`
- All CI checks passing: `cargo fmt`, `clippy -D warnings`, `test`, `doc`
