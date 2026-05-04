# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Koda is a personal AI assistant built in Rust (edition 2024) — not a product,
not a platform. See [DESIGN.md](DESIGN.md) for principles (P1: Personal,
P2: Simple enough to own alone, P3: Build for the world six months from now).

**Development philosophy:** Make it work, make it right, make it fast — in that
order. Ship working code first, refactor to clean design second, optimize for
performance only when measured.

Two-crate workspace:
- `koda-core` (library) — pure engine with zero terminal deps
- `koda-cli` (binary `koda`) — CLI frontend with ratatui TUI

See [DESIGN.md](DESIGN.md) for architectural decisions. See [#70](https://github.com/lijunzh/koda/issues/70) for the TUI design.

### Current Architecture

Simple inference loop: stream LLM response → execute tool calls → repeat.
No phases, no tiers — the model drives execution directly.

- **Context from API** (`providers/`): queries actual context window from provider
  - Fallback: `model_context.rs` lookup table
- **Rate limit retry**: exponential backoff (2/4/8/16/32s) for 429 errors
- **Built-in agents**: default (others via user-created agent configs)
- **Git checkpointing** (`git.rs`): auto-snapshot before each turn

Approval is per-tool. A single `TrustMode` enum (Plan/Safe/Auto) controls
how mutations are gated:

- **TrustMode** (`trust.rs`): Plan (deny writes), Safe (confirm), Auto (auto-approve)
- **ToolEffect** (`tools/mod.rs`): ReadOnly / LocalMutation / Destructive / RemoteAction
  - Plan: only ReadOnly allowed; all mutations denied
  - Safe: local mutations and destructive ops need confirmation
  - Auto: everything auto-approved; sandbox enforces the perimeter
- **Hardcoded floor**: outside-project writes always need confirmation
  regardless of mode
- **Folder scoping** (`trust.rs`, `bash_path_lint.rs`):
  - `is_outside_project()`: checks file tool paths against project_root
  - `lint_bash_paths()`: heuristic bash command analysis for cd/path escapes
  - Startup warning when project_root == $HOME
- **Process sandbox** (`sandbox.rs`): always-on kernel sandbox
  - Writes restricted to project + /tmp + cache dirs
  - Credential dirs blocked (~/.ssh, ~/.aws, etc.)
  - macOS: `sandbox-exec -p` (seatbelt); Linux: `bwrap` (bubblewrap)
  - Graceful fallback with `tracing::warn!` if backend unavailable
  - Sub-agents inherit via `TrustMode::clamp()` — child can never weaken parent

## Documentation Rules

**When to update docs with a PR:**
- User-facing feature added/changed → update root README + relevant crate README
- Architecture or design decision → add to the appropriate section in `DESIGN.md` with rationale
- New crate → must ship with a README.md (required for crates.io)
- Internal refactors don't require doc updates unless they change crate boundaries or public APIs

**On release:**
- Move CHANGELOG.md `[Unreleased]` to versioned section
- Bump version in both crate Cargo.toml files (koda-core, koda-cli)
- Verify README quick-start examples still work
- Check that CHANGELOG entries match what's documented in README/DESIGN.md

## Build & Development Commands

```bash
cargo build                              # Debug build (all crates)
cargo build --release -p koda-cli        # Release build (CLI only)
cargo test --workspace --features koda-core/test-support  # Run all tests (incl. E2E)
cargo test -p koda-core --features test-support          # Engine tests only
cargo test -p koda-cli                                   # CLI tests only
cargo test -p koda-core --test perf_test                 # Run a specific test file
cargo fmt --all                          # Format all crates
cargo fmt --all --check                                                    # Check formatting (CI enforced)
cargo clippy --workspace --all-targets --features koda-core/test-support -- -D warnings  # Lint (CI enforced)
cargo check --workspace --all-targets                                       # Compile-check all targets, no features (CI enforced, ubuntu + windows)
cargo doc --workspace --no-deps                                             # Build docs
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
│   │   ├── approval.rs     # Approval flow helpers (delegates to trust.rs)
│   │   ├── bash_path_lint.rs # Heuristic path-escape detection for bash commands
│   │   ├── bash_safety.rs  # Bash command safety classification
│   │   ├── bg_agent.rs     # Background sub-agent registry
│   │   ├── compact.rs      # Session compaction (summarize old messages)
│   │   ├── config.rs       # Agent/provider config + provider metadata
│   │   ├── context.rs      # Context window token tracking
│   │   ├── db/             # SQLite persistence (split into submodules)
│   │   │   ├── mod.rs      # Database struct, migrations, schema (WAL mode)
│   │   │   ├── queries.rs  # Persistence trait impl (all SQL queries)
│   │   │   └── tests.rs    # Database integration tests
│   │   ├── file_tracker.rs # File lifecycle tracking (create/edit/delete ownership)
│   │   ├── git.rs          # Git checkpointing + rollback
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── inference_helpers.rs # Token estimation, message assembly, overflow detection
│   │   ├── keystore.rs     # Secure API key storage (~/.config/koda/keys.toml, 0600)
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── memory.rs       # Semantic memory (global + project tiers → system prompt)
│   │   ├── microcompact.rs # Lightweight tool result aging between full compactions
│   │   ├── model_alias.rs  # Curated model aliases (e.g. "sonnet" → exact model ID)
│   │   ├── model_context.rs# Model → context window size lookup table (fallback)
│   │   ├── output_caps.rs  # Output cap scaling based on context window size
│   │   ├── persistence.rs  # Persistence trait — the database contract
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── progress.rs     # Progress reporting helpers for long operations
│   │   ├── prompt.rs       # System prompt construction
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── sandbox.rs      # Process sandbox (macOS seatbelt + Linux bwrap, always-on)
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── last_provider.rs # Last-used provider recall (SQLite KV, not a config file)
│   │   ├── skills.rs       # Skill discovery and activation
│   │   ├── skill_scope.rs  # Skill-scoped tool allow/deny enforcement
│   │   ├── sub_agent_cache.rs # Sub-agent provider/model config cache
│   │   ├── tool_dispatch.rs# Tool dispatch — routes tool calls to the registry
│   │   ├── tool_normalize.rs # Tool name normalization (snake_case → PascalCase)
│   │   ├── approval_flow.rs# Approval logic extracted from tool dispatch
│   │   ├── trust.rs        # TrustMode (Plan/Safe/Auto), ToolApproval, check_tool()
│   │   ├── sub_agent_dispatch.rs # Sub-agent invocation dispatch
│   │   ├── context_analysis.rs # Per-tool token breakdown + duplicate detection
│   │   ├── truncate.rs     # Token-safe output truncation
│   │   ├── undo.rs         # Undo stack for file mutations
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── worktree.rs     # Git worktree isolation for sub-agents
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   │   ├── mod.rs      # Module root + re-exports
│   │   │   ├── event.rs    # EngineEvent + EngineCommand protocol types
│   │   │   └── sink.rs     # EngineSink trait (how clients receive events)
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
│   │       ├── ask_user.rs # AskUser tool (explicit user clarification)
│   │       ├── bg_process.rs # Background process registry + spawning
│   │       ├── file_tools.rs # Read, Write, Edit, Delete
│   │       ├── fuzzy.rs    # Fuzzy old_str matching for Edit
│   │       ├── glob_tool.rs# Glob file search
│   │       ├── grep.rs     # Ripgrep-based text search
│   │       ├── memory.rs   # MemoryRead, MemoryWrite
│   │       ├── recall.rs   # RecallContext (session history)
│   │       ├── shell.rs    # Bash command execution
│   │       ├── skill_tools.rs # ListSkills, ActivateSkill
│   │       ├── todo.rs     # TodoWrite
│   │       ├── validate.rs # Pre-flight input validation
│   │       ├── web_fetch.rs# WebFetch (URL content retrieval)
│   │       └── web_search.rs # WebSearch (DuckDuckGo)
│   └── tests/              # Engine integration tests
├── koda-cli/               # CLI binary
│   ├── src/
│   │   ├── main.rs         # CLI entry point (clap)
│   │   ├── lib.rs          # Crate root (exports acp_adapter)
│   │   ├── tui_app.rs      # Main TUI event loop (ratatui Viewport::Fullscreen)
│   │   ├── app.rs          # App struct: aggregated TUI state + lifecycle
│   │   ├── builtin_skills.rs # Built-in skill definitions (URL index)
│   │   ├── tui_context/    # TUI shared mutable state (split into submodules)
│   │   │   ├── mod.rs      # TuiContext struct, event loop, command dispatch
│   │   │   ├── events.rs   # Key/mouse handling, clipboard, history persistence
│   │   │   └── menus.rs    # Model/provider/session pickers, provider wizard
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
│   │   ├── transcript.rs   # Session transcript renderer (Markdown export)
│   │   ├── headless.rs     # Single-prompt headless mode
│   │   ├── sink.rs         # CliSink (unbounded channel forwarding for TUI)
│   │   ├── server.rs       # ACP server over stdio JSON-RPC
│   │   ├── acp_adapter.rs  # ACP protocol adapter
│   │   ├── onboarding.rs   # First-run wizard (provider + API key setup)
│   │   └── tool_history.rs # Tool output history for /expand
│   │   ├── wrap_input.rs   # Input wrapping and paste detection
│   │   └── wrap_util.rs    # Visual line-width utilities
│   │   └── widgets/        # TUI widgets
│   │       ├── mod.rs      # Widget module root
│   │       ├── dropdown.rs # Generic dropdown widget
│   │       ├── file_menu.rs# @file autocomplete menu
│   │       ├── model_menu.rs # Model picker menu
│   │       ├── provider_menu.rs # Provider picker menu
│   │       ├── session_menu.rs # Session picker menu
│   │       ├── slash_menu.rs# Slash command dropdown (see DESIGN.md, Interaction)
│   │       └── status_bar.rs# Model, mode, context meter, elapsed time
│   └── tests/              # CLI integration tests
├── DESIGN.md               # Architecture decisions
```

### Core Event Loop

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` → `inference_loop()` (streaming LLM + tools)

The TUI uses `ratatui::Viewport::Fullscreen` (alternate screen buffer) with
app-managed scrollback. All output flows through a single rendering path:
`EngineEvent` → `tui_render.rs` → `ScrollBuffer` → `draw_viewport()`.
Mouse capture is enabled for scroll wheel; text selection is handled by
`mouse_select.rs` with automatic clipboard copy.

**Viewport layout** (see DESIGN.md, Interaction):
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

See DESIGN.md (Interaction section) for the interaction system design and competitive analysis.

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

### Bash tool: pass multi-line / backtick content via files, not inline

The Bash tool runs commands through `sh -c "…"`. Inlining content that
contains newlines, backticks, `$(…)`, or shell metacharacters in the
argument string burns a turn on `sh: -c: line N: syntax error near
unexpected token …` (#1232 §7).

**Always-good pattern for `gh` issue/PR creation, commit messages,
and any other multi-line payload:**

```bash
# 1. write the body to a temp file
cat > /tmp/pr_body.md <<'EOF'
## Summary

Multi-line content with `backticks`, $(commands), and "quotes" all
survive because the heredoc is parsed BEFORE sh -c sees the arg.
EOF

# 2. reference it via --body-file / -F
gh pr create --title "…" --body-file /tmp/pr_body.md
gh issue create --title "…" --body-file /tmp/pr_body.md
git commit -F /tmp/commit_msg.md
```

The heredoc is single-quoted (`<<'EOF'`) so nothing inside it is
expanded — you don't have to escape `$`, backticks, or `"`. This is
strictly safer than `--body "…"` even for content you think is safe;
the foot-gun is recurring and has zero upside to ignoring.

## Test Structure

### Quick reference

```bash
# CI suite (all tests including E2E with mock provider)
cargo test --workspace --features koda-core/test-support

# Individual crate tests
cargo test -p koda-core --features test-support   # Engine (unit + E2E)
cargo test -p koda-cli                             # CLI (unit + integration)

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
- `tests/e2e_harness/mod.rs` — shared `Env` test harness (temp dir, DB, config)
- `tests/e2e_test.rs` — core pipeline: text streaming, tool calls, errors, history, cancel
- `tests/e2e_tools_test.rs` — tool coverage: Glob, Grep, Edit, Delete, AST verification
- `tests/e2e_agent_test.rs` — sub-agent invocation + cache hit
- `tests/e2e_skills_test.rs` — skills, compaction, file ownership
- `tests/cancel_test.rs` — Ctrl+C interruption during inference

**Integration tests** — no feature flag:
- `tests/file_tools_test.rs` — path safety, file CRUD
- `tests/new_tools_test.rs` — glob, tool naming
- `tests/guarantee_matrix_test.rs` — trust mode × tool effect matrix
- `tests/inference_recovery_test.rs` — rate-limit retry, context overflow
- `tests/inference_edge_test.rs` — loop detection, empty tool calls
- `tests/e2e_safety_test.rs` — approval gates, bash safety classification
- `tests/session_test.rs` — session lifecycle, TurnStart/TurnEnd events
- `tests/purge_test.rs` — /purge feature (compacted stats, age filter)
- `tests/perf_test.rs` — DB, grep, markdown throughput
- `tests/capabilities_test.rs` — capabilities.md freshness
- `tests/tool_wiring_test.rs` — every tool routable + approval-handled
- `tests/tool_normalize_test.rs` — snake_case → PascalCase normalization
- `tests/golden_test.rs` — snapshot / golden file tests
- `tests/snapshot_test.rs` — tool definition schema snapshots
- `tests/token_audit.rs` — token estimation accuracy checks

#### koda-cli

**Unit tests** — co-located in `src/`:
- Markdown rendering, REPL parsing, highlighting

**Integration tests** — no feature flag:
- `tests/cli_test.rs` — binary subprocess invocation
- `tests/regression_test.rs` — REPL dispatch, input processing
- `tests/server_test.rs` — ACP server integration (JSON-RPC lifecycle)
- `tests/skill_agent_e2e_test.rs` — skill activation + agent switching E2E

**Smoke tests** (MockProvider, CI-safe):
- `tests/smoke_test.rs` — headless mode: text responses, tool use, session resume, `--resume` flag
- Gated by `KODA_TEST_LMSTUDIO=1` env var; never runs in CI

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
10. Sync version with workspace (currently 0.3.0)
11. Update this file (CLAUDE.md)

## CI Workflows

Four workflows live in `.github/workflows/`:

| Workflow | Trigger | Purpose |
|---|---|---|
| `ci.yml` | Pull requests to `main` | Gate PRs — lint, cross-platform check, test, doc, audit |
| `coverage.yml` | Push to `main` | Post-merge coverage tracking with auto issue creation |
| `release.yml` | Tag `v*` | Version verify, cross-platform test + build, publish, Homebrew |

Release job chain:
```
verify-version → test → build → github-release ─┬→ update-homebrew
                                                 └→ publish (crates.io)
```
crates.io and Homebrew are independent distribution channels — both fan
out from `github-release` in parallel. Publishing to crates.io only after
`github-release` ensures binaries are confirmed good before committing to
an immutable version number; it does not need to wait for Homebrew.
| `docs.yml` | Push to `main` (docs/ changes) | Build + deploy mdBook to GitHub Pages |

### CI job DAG (`ci.yml`)

```
test (fmt → clippy → check → test)   single container, compile once
doc        (independent)              catches broken rustdoc regardless of test
audit      (independent)              reads Cargo.lock against advisories — orthogonal to code quality
```

- `test` runs all checks sequentially in one container: `cargo fmt --check` → `cargo clippy`
  → `cargo check --all-targets` (no features, catches cfg-gated gaps) → `cargo test`.
  A failure in any step aborts the rest — fast-fail is preserved within the job.
- One container = one compile chain = no redundant recompilation across separate runners.
  The pre-push hook locally runs only `cargo fmt --check` + `cargo clippy --workspace`
  (the two cheap checks) — CI is the source of truth for tests, doc, and audit.
- `doc`, `audit` run unconditionally in parallel — independent signals, fast enough that
  cancelling them on test failure would lose information, not save time.

**Branch ruleset required gates: `Test` and `Docs` only.**
`Docs` is required separately because it is independent of the `test` job.

### Coverage workflow (`coverage.yml`)

Coverage is **not** a PR gate — it runs post-merge on `main` and is informational.

| Crate | Threshold |
|---|---|
| `koda-core` | ≥ 80% line coverage |

**Regression handling** — when any crate drops below threshold:
1. Workflow run turns red → GitHub notifies via email.
2. A GitHub issue is automatically created with label `coverage-regression`,
   commit SHA, run link, and which crates failed.
3. If an open `coverage-regression` issue already exists, a comment is added
   instead (no spam). **The label is exclusively owned by `coverage.yml` —
   do not apply it manually or from another workflow, as this breaks deduplication.**
4. When coverage recovers, the open issue is auto-closed with a comment.

**Why not a PR gate?** Coverage reruns the full test suite with instrumentation
(2–5× slower). Blocking PRs doubles the test cost and slows feedback. The
fix-forward model (merge → notify → fix PR) keeps PRs fast while maintaining
accountability on `main`.

