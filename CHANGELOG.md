# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.1.0] - 2026-03-03

First published release of `koda-core` and `koda-cli` as separate crates.
v0.1.x was an intentional simplified prototype to test the feasibility of
building a high-performance AI coding agent in Rust. With the architecture
validated, v0.2.0 will evolve Koda into a modern client-server platform.

✨ **Highlights:** Workspace split · Channel-based approval · KodaAgent/KodaSession structs · CI/CD for dual crate publishing

### Architecture
- **Workspace split**: Single `koda-agent` crate → `koda-core` (library) + `koda-cli` (binary)
  - `koda-core`: pure engine with zero terminal dependencies
  - `koda-cli`: CLI frontend, produces the `koda` binary
  - `cargo install koda-cli` replaces `cargo install koda-agent`
- **Channel-based approval**: `EngineSink::request_approval()` removed. Approval now flows through async `EngineEvent::ApprovalRequest` + `EngineCommand::ApprovalResponse` over `tokio::mpsc` channels — works over any transport.
- **CancellationToken**: Replaces global `AtomicBool` interrupt flag. Proper per-session cancellation.
- **KodaAgent**: Shared, immutable agent resources (tools, system prompt, MCP registry). `Arc`-shareable for parallel sub-agents.
- **KodaSession**: Per-conversation state (DB, provider, settings, cancel token). `run_turn()` replaces 15-parameter `inference_loop()` call.
- **App split**: `app.rs` (1127 lines) → `app.rs` (602) + `headless.rs` (88) + `commands.rs` (486)

### Testing
- Integration tests split by crate boundary: engine tests in `koda-core/tests/`, CLI tests in `koda-cli/tests/`
- Recovered 55 orphaned integration tests from workspace migration
- 347 tests total (was 293 in v0.1.4)

### CI/CD
- Updated for workspace: `cargo check/test/clippy --workspace`
- Dual crate publishing: `koda-core` published first, then `koda-cli` (with 30s index delay)
- Version verification checks both crate versions match the git tag

### Documentation
- README: updated install command (`koda-cli`), architecture section, prototype status
- DESIGN.md: updated to reflect actual workspace structure and implementation phases
- CLAUDE.md: rewritten for workspace layout, new types, test locations
- CHANGELOG: added v0.1.4 and v0.1.5 entries

## [0.1.4] - 2026-03-03

✨ **Highlights:** Engine extraction · EngineEvent/EngineCommand protocol · EngineSink trait

### Architecture
- **EngineEvent** (18 variants) + **EngineCommand** (5 variants): JSON serde-ready protocol types defining the engine ↔ client boundary
- **EngineSink** trait with CliSink (terminal) and TestSink (testing)
- `inference.rs` fully decoupled from display/markdown/confirm modules
- Approval flow routed through EngineSink
- `<think>` tag parsing moved to provider layer (ThinkTagFilter)
- Markdown streaming wired through CliSink

This release introduces major leaps in model interoperability, significantly reduces token overhead, and lays the groundwork for external tool integration via the Model Context Protocol. It focuses on giving developers more flexibility with local providers and maintaining high performance over long sessions.

✨ **Highlights:** MCP Support · OpenAI-Compatible Providers · Gemini Reasoning · 61% System Prompt Optimization

### Added
- **Model Context Protocol (MCP)**: Full support for external tools via MCP, configurable via `.mcp.json` and interactive `/mcp` commands.
- **OpenAI-Compatible Providers**: Connect to Ollama, DeepSeek, Mistral, and other local models seamlessly.
- **Custom Local URLs**: Route local LLM requests to custom proxy URLs via interactive setup or the `KODA_LOCAL_URL` environment variable.
- **Gemini Thinking Support**: Added support for Gemini's native reasoning capabilities, including rendering the `thought_signature` in tool histories.
- **Session Tasks**: Introduced the `TodoWrite` tool for session-scoped task tracking.

### Changed
- **Performance**: Optimized the core system prompt, achieving a 61% token reduction (1,638 → 632 tokens) for massive cost savings on long sessions.
- **Database Architecture**: Centralized the SQLite database to `~/.config/koda/koda.db` for cleaner workspace management.
- **Documentation**: Consolidated various markdown files into `DESIGN.md` and migrated feature requests directly to GitHub issues.

### Fixed
- **Memory Consistency**: Ensure the `/memory save` command respects the active context file (e.g., `CLAUDE.md`).
- **Formatting & Linting**: Resolved multiple CI failures, dead code, and collapsible `if` statement warnings in `config.rs`.

## [0.1.2] - 2026-03-02

✨ **Highlights:** approval modes (plan/normal/yolo) · diff preview before confirmation · loop detection · native Gemini provider · extended thinking display

### Added
- **Approval modes** (`/trust`): Plan (read-only), Normal (smart confirm), Yolo (auto-approve)
  - Interactive picker via `/trust` command
  - `Shift+Tab` cycles modes inline; mode name shown in prompt
  - Bash safety classification: parses pipelines against safe-command whitelist
- **Diff preview**: Edit, Write, and Delete tools show a unified diff before confirmation
- **Loop detection**: detects repeated identical tool calls and prompts the user
  - Configurable hard cap on iterations with interactive extend-or-stop
- **Native Gemini provider**: direct Google AI API (replaces OpenAI-compat shim)
- **Structured thinking renderer**: extended thinking blocks display with violet `│` gutter
- **Prompt token count** shown in response footer alongside completion tokens
- **Model settings**: extended thinking budget and reasoning effort configuration
- **Anthropic dynamic model list**: fetches from `/v1/models` endpoint instead of hardcoded list
- **Stale-read optimization**: skips re-reading files the LLM just wrote
- `/trust` and `/exit` added to `/help` menu

### Fixed
- **Plan mode redesign**: Plan is now read-only (not do-nothing). Can read files,
  grep, run safe bash, invoke sub-agents, and fetch URLs — only write ops are blocked.
- **Sub-agent approval bypass**: sub-agents now inherit the parent's approval mode
  (plan/normal/yolo). Previously sub-agents could run Write/Delete/Bash without confirmation.
- **Shell injection patterns**: `$(...)`, backticks, and `eval` are now classified as dangerous
  and require confirmation in Normal mode
- **Full command in confirmation**: confirmation prompt now shows the full untruncated shell
  command (banner stays truncated for clean visual scanning)
- Bash tool now refuses to run `grep`, `cat`, `find`, `ls` (use built-in tools instead)
- `Esc` clears the input line; `Ctrl-C` clears input or exits when empty
- Suppress empty "Response" banner when LLM only returns tool calls
- Warn when model silently exits mid-task after tool use
- Shell tool output display cleaned up
- BackTab keybinding panic from radix_trie collision
- Mode switch updates prompt in-place (no stacking lines)
- Plan mode log uses prompt icon (📋) and yellow color

### Removed
- Dead code cleanup and stale doc reconciliation

## [0.1.1] - 2026-03-02

### Performance
- Release binary size reduced from 14MB to 7.6MB (47% smaller)
  - `strip = true` removes debug symbols
  - `lto = true` enables link-time optimization across crates
  - `codegen-units = 1` improves dead-code elimination
  - `panic = "abort"` removes unwinding machinery

### Removed
- Deleted `clipboard.rs` (dead code since `/copy` removal)
- Removed unused `ToolResult.success` field
- Removed unused `keystore::get()`, `keystore::remove()`, `runtime_env::remove()`

### Fixed
- Windows: drag-and-drop image detection now recognizes `C:\` drive paths

### Docs
- README trimmed from 295 to 77 lines; detailed docs moved to DESIGN.md/FUTURE.md

### CI/CD
- Homebrew tap auto-update on release (`lijunzh/homebrew-koda`)
- Install: `brew tap lijunzh/koda && brew install koda`

---

## [0.1.0] - 2026-03-01

**A high-performance AI coding agent built in Rust.** Single binary. Multi-provider. Zero dependencies.

✨ **Highlights:** 14 tools · 6 LLM providers · 5 embedded agents · parallel sub-agents · image analysis · headless mode · auto-compact · prompt caching

### Core
- 14 built-in tools: file ops (Read/Write/Edit/Delete/List), search (Grep/Glob),
  shell (Bash), web (WebFetch), memory (MemoryRead/Write),
  agents (InvokeAgent/ListAgents/CreateAgent)
- 6 LLM providers: LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok
- Streaming responses with inline markdown rendering and syntax highlighting
- SQLite-backed durable sessions with session management
- Smart input: tab completion, ghost hints, @file context injection
- Interactive TUI menus for model/provider selection
- Tool confirmation system for destructive operations
- Safe path validation preventing directory traversal attacks
- Onboarding wizard for first-run setup

### Agents
- 5 built-in agents embedded at compile time (zero disk dependency):
  - **default** — main coding assistant
  - **reviewer** — critical code reviewer (read-only, severity-tagged)
  - **security** — security auditor (OWASP, CWE-tagged)
  - **testgen** — QA engineer (finds gaps, writes tests)
  - **releaser** — Release engineer (discover → plan → execute workflow)
- Agent discovery merges built-in + user (~/.config/koda/agents/) + project (agents/)
- CreateAgent tool with validation (name, duplicates, prompt quality)
- `/agent` command to list available sub-agents with source tags

### Parallel Execution
- Parallel tool execution via `futures::join_all` when no confirmation needed
- Enables concurrent sub-agent invocation (reviewer + security + testgen)

### Image Analysis
- Multi-modal image support via `@image.png` references
- Drag-and-drop: auto-detects bare image paths (absolute, ~/,  ./,  quoted)
- Provider-specific formats: OpenAI image_url data URIs, Anthropic image blocks
- Supported formats: PNG, JPEG, GIF, WebP, BMP

### Context Management
- Auto-compact at 80% context usage (LLM summarizes conversation)
- Manual `/compact` command
- Sliding window safety net for token budget
- Context percentage shown in prompt at ≥75%

### Headless Mode
- `koda -p "prompt"` or `koda "prompt"` for one-shot execution
- Piped stdin auto-detection: `echo "explain" | koda`
- `--output-format json` for CI/CD integration
- Exit code 0/1 for scripting

### Shell Output Management
- Display: last 20 lines shown to user (clean, predictable)
- LLM context: last 256 lines kept (sufficient for analysis)
- Per-line cap: 256 characters
- Tool description teaches LLM to pipe and filter output

### Git Integration
- `/diff` — show uncommitted changes summary
- `/diff review` — LLM code review of uncommitted changes
- `/diff commit` — generate conventional commit messages

### Performance
- Anthropic prompt caching: system prompt + tools cached (90% cheaper input tokens)
- Lean system prompt: 807 tokens (tool guidance lives in tool descriptions)
- Sub-agent prompts optimized: principle-based, language-agnostic

### Memory & Persistence
- Project memory: MEMORY.md (also reads CLAUDE.md, AGENTS.md)
- Global memory: ~/.config/koda/memory.md
- Persistent API keys: ~/.config/koda/keys.toml
- REPL history: ~/.config/koda/history

### REPL Commands
- `/agent` — list available sub-agents
- `/compact` — summarize conversation to reclaim context
- `/cost` — show token usage for the session
- `/diff` — show/review uncommitted changes, generate commit messages
- `/help` — command palette
- `/memory` — view/save project and global memory
- `/model` — switch models interactively
- `/provider` — switch LLM providers
- `/sessions` — list, resume, or delete sessions
- Keyboard shortcuts: `Ctrl+C` to interrupt, `Ctrl+D` to exit, `@file` to attach context

### CI/CD
- GitHub Actions: CI (fmt, clippy, test ×3 OS, doc, audit)
- Release pipeline: tag → CI gate → build (5 targets) → crates.io + GitHub release
- Release notes auto-extracted from CHANGELOG.md
- Dependabot for cargo and GitHub Actions dependencies

### Testing
- 288 tests across 6 suites
- All CI checks passing: cargo fmt, clippy -D warnings, test, doc
