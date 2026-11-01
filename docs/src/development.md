# Development reference

Reference for working on Koda itself. [`CLAUDE.md`](https://github.com/lijunzh/koda/blob/main/CLAUDE.md)
holds what an agent needs on every turn; this page holds what it needs only for
a particular job: the full workspace tree, the release and new-capability
checklists, and how CI, the security audit and coverage work.

## Workspace

```
koda/
├── Cargo.toml              # Workspace root
├── koda-core/              # Engine library (zero terminal deps)
│   ├── agents/             # Built-in agent configs (default, explore, plan, verify, task)
│   ├── skills/             # Built-in skills (SKILL.md files)
│   ├── src/
│   │   ├── lib.rs          # Crate root
│   │   ├── agent/          # KodaAgent (shared config: tools, prompt)
│   │   ├── approval_flow.rs# Approval logic extracted from tool dispatch
│   │   ├── bash_path_lint.rs # Heuristic path-escape detection for bash commands
│   │   ├── bash_safety.rs  # Bash command safety classification
│   │   ├── child_agent.rs  # Background sub-agent registry
│   │   ├── compact.rs      # Session compaction (summarize old messages)
│   │   ├── config.rs       # Agent/provider config + provider metadata
│   │   ├── context.rs      # Context window token tracking
│   │   ├── context_analysis.rs # Per-tool token breakdown + duplicate detection
│   │   ├── db/             # SQLite persistence (split into submodules)
│   │   │   ├── mod.rs      # Database struct, migrations, schema (WAL mode)
│   │   │   ├── context_cache.rs # Per-session context cache
│   │   │   ├── queries.rs  # Persistence trait impl (all SQL queries)
│   │   │   └── tests.rs    # Database integration tests
│   │   ├── file_tracker.rs # File lifecycle tracking (create/edit/delete ownership)
│   │   ├── git.rs          # Read-only git context for the system prompt (branch, diff, recent commits)
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── inference_helpers.rs # Token estimation, message assembly, overflow detection
│   │   ├── instructions.md # Behavioral instructions in the system prompt
│   │   ├── keystore.rs     # Secure API key storage (SQLite KV store, apikey: prefix)
│   │   ├── last_provider.rs # Last-used provider recall (SQLite KV, not a config file)
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── mcp/            # MCP client: config, manager, tool bridge
│   │   ├── memory.rs       # Semantic memory (global + project tiers → system prompt)
│   │   ├── microcompact.rs # Lightweight tool result aging between full compactions
│   │   ├── model_alias.rs  # Curated model aliases (e.g. "sonnet" → exact model ID)
│   │   ├── model_context.rs# Model → context window size lookup table (fallback)
│   │   ├── output_caps.rs  # Output cap scaling based on context window size
│   │   ├── persistence.rs  # Persistence trait — the database contract
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── prompt.rs       # System prompt construction
│   │   ├── provider_catalog.rs # Static metadata for every supported provider
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── sandbox.rs      # Process sandbox (macOS seatbelt + Linux bwrap, always-on)
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── skills.rs       # Skill discovery and activation
│   │   ├── skill_scope.rs  # Skill-scoped tool allow/deny enforcement
│   │   ├── sub_agent_cache.rs # Sub-agent provider/model config cache
│   │   ├── sub_agent_dispatch.rs # Sub-agent invocation dispatch
│   │   ├── tool_dispatch.rs# Tool dispatch — routes tool calls to the registry
│   │   ├── tool_normalize.rs # Tool name normalization (snake_case → PascalCase)
│   │   ├── trust.rs        # TrustMode (Plan/Safe/Auto), ToolApproval, check_tool()
│   │   ├── truncate.rs     # Token-safe output truncation
│   │   ├── turn_context.rs # TurnContext and ToolExecutionContext bundles
│   │   ├── undo.rs         # Undo stack for file mutations
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   │   ├── mod.rs      # Module root + re-exports
│   │   │   ├── event.rs    # EngineEvent + EngineCommand protocol types
│   │   │   └── sink.rs     # EngineSink trait (how clients receive events)
│   │   ├── providers/      # LLM providers
│   │   │   ├── mod.rs      # LlmProvider trait, ChatMessage, HTTP client builder
│   │   │   ├── anthropic.rs# Anthropic Claude (prompt caching, extended thinking)
│   │   │   ├── gemini.rs   # Google Gemini (context caching, key-in-URL)
│   │   │   ├── http.rs     # Provider-neutral HTTP helpers
│   │   │   ├── openai_compat.rs # OpenAI-compatible (GPT, DeepSeek, local models)
│   │   │   ├── mock.rs     # Mock provider for tests
│   │   │   ├── stream_collector.rs # ChunkParser trait + shared SSE stream collector
│   │   │   └── stream_tag_filter.rs # Strip XML tool tags from streaming text
│   │   └── tools/          # Built-in tools
│   │       ├── mod.rs      # ToolRegistry, ToolEffect, tool definitions
│   │       ├── agent.rs    # Sub-agent invocation tool
│   │       ├── ask_user.rs # AskUser tool (explicit user clarification)
│   │       ├── bg_process.rs # Background process registry + spawning
│   │       ├── cargo.rs    # Cargo tool: structured compiler and test feedback
│   │       ├── catalog.rs  # Read-only tool metadata catalog
│   │       ├── file_tools.rs # Read, Write, Edit, Delete
│   │       ├── fuzzy.rs    # Fuzzy old_str matching for Edit
│   │       ├── glob_tool.rs# Glob file search
│   │       ├── grep.rs     # Ripgrep-based text search
│   │       ├── memory.rs   # MemoryRead, MemoryWrite
│   │       ├── recall.rs   # RecallContext (session history)
│   │       ├── shell.rs    # Bash command execution
│   │       ├── skill_tools.rs # ListSkills, ActivateSkill
│   │       ├── spawn_agent.rs # SpawnAgent
│   │       ├── summary.rs  # What each tool call displays
│   │       ├── task_id.rs  # Background-task ID parsing
│   │       ├── todo.rs     # TodoWrite
│   │       ├── tool_trait.rs # Tool trait and per-invocation context
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
│   │   ├── child_activity.rs # Live tracker for background-work activity
│   │   ├── clipboard.rs    # Clipboard abstraction
│   │   ├── composer/       # Input box: textarea, paste bursts, history, slash popup (SLASH_COMMANDS)
│   │   ├── debug_bundle/   # /debug-bundle slash command
│   │   ├── frame_requester.rs # Frame draw scheduling
│   │   ├── hyperlink.rs    # OSC 8 hyperlink post-render pass
│   │   ├── inference/      # Inference turn lifecycle: inner event loop + post-turn cleanup
│   │   ├── panic_log.rs    # Forensic record of a panic in ~/.config/koda/logs/panic.log
│   │   ├── queue_lanes.rs  # Helpers for the TUI's two input queue lanes
│   │   ├── theme.rs        # Semantic color tokens
│   │   ├── tui_context/    # TUI shared mutable state (split into submodules)
│   │   │   ├── mod.rs      # TuiContext struct, event loop, command dispatch
│   │   │   ├── events.rs   # Key/mouse handling, clipboard, history persistence
│   │   │   ├── idle_ui_events.rs # UI events while no turn is running
│   │   │   └── menus.rs    # Model/provider/session pickers, provider wizard
│   │   ├── tui_render.rs   # EngineEvent → ratatui Line/Span rendering
│   │   ├── tui_commands.rs # Slash command dispatch (/help, /model, /sessions, etc.)
│   │   ├── tui_bg_tasks.rs # /agents and /cancel slash commands
│   │   ├── tui_mcp.rs      # /mcp slash commands
│   │   ├── tui_wizards.rs  # Interactive wizards (/provider, /compact, /agent)
│   │   ├── tui_output.rs   # Output bridge: scroll_buffer push helpers
│   │   ├── tui_viewport.rs # Fullscreen viewport layout + rendering
│   │   ├── tui_types.rs    # MenuContent, PromptMode, TuiState, shared TUI types
│   │   ├── tool_header.rs  # Tool-call header rendering
│   │   ├── tool_output_lines.rs # Per-line tool output rendering
│   │   ├── scroll_buffer.rs# App-managed scrollback buffer with visual-line scroll
│   │   ├── mouse_select.rs # Mouse text selection + clipboard copy
│   │   ├── ansi_parse.rs   # ANSI escape code → ratatui Span conversion
│   │   ├── history_render.rs # Session history → scroll_buffer replay
│   │   ├── md_render.rs    # Streaming markdown → ratatui renderer
│   │   ├── completer.rs    # Tab completion (/commands, @files, /model names)
│   │   ├── diff_render.rs  # Diff preview → ratatui renderer (syntax highlighted)
│   │   ├── highlight.rs    # Syntax highlighting via syntect
│   │   ├── startup.rs      # Startup banner rendering
│   │   ├── input.rs        # @file reference processing + image loading
│   │   ├── headless.rs     # Single-prompt headless mode
│   │   ├── sink.rs         # CliSink (unbounded channel forwarding for TUI)
│   │   ├── server.rs       # ACP server over stdio JSON-RPC
│   │   ├── acp_adapter.rs  # ACP protocol adapter
│   │   ├── onboarding.rs   # First-run wizard (provider + API key setup)
│   │   ├── tool_history.rs # Tool output history for /expand
│   │   ├── util.rs         # Shared utilities
│   │   ├── wrap_util.rs    # Visual line-width utilities
│   │   └── widgets/        # TUI widgets
│   │       ├── mod.rs      # Widget module root
│   │       ├── child_activity_overlay.rs # Background-work overlay
│   │       ├── dropdown.rs # Generic dropdown widget
│   │       ├── file_menu.rs# @file autocomplete menu
│   │       ├── model_menu.rs # Model picker menu
│   │       ├── provider_menu.rs # Provider picker menu
│   │       ├── queue_preview.rs # Queued-input preview
│   │       ├── session_menu.rs # Session picker menu
│   │       ├── shortcuts_overlay.rs # Keybinding reference overlay
│   │       └── status_bar.rs# Model, mode, context meter, elapsed time
│   └── tests/              # CLI integration tests
├── koda-sandbox/           # Sandbox runtime: kernel-level FS, network and exec policies
├── koda-test-utils/        # Shared test helpers
├── DESIGN.md               # Architecture decisions
```

## Core event loop

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` →
`inference_loop()` (streaming LLM + tools)

The TUI uses `ratatui::Viewport::Fullscreen` (alternate screen buffer) with
app-managed scrollback. All output flows through a single rendering path:
`EngineEvent` → `tui_render.rs` → `ScrollBuffer` → `draw_viewport()`. Mouse
capture is enabled for scroll wheel; text selection is handled by
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
remaining vertical space. Mouse scroll works during inference. Native clipboard
(`arboard`) handles copy since alternate screen disables terminal-native mouse
selection. DESIGN.md (Interaction section) has the interaction system design
and competitive analysis.

The engine communicates through `EngineEvent` (output) and `EngineCommand`
(input) enums. Approval flows through async channels: the engine emits
`ApprovalRequest`, the client sends `ApprovalResponse`. See
[#70](https://github.com/lijunzh/koda/issues/70) for the TUI design.

## Engine notes

- **Context from API** (`providers/`): queries the actual context window from
  the provider. Fallback: the `model_context.rs` lookup table.
- **Rate limit retry:** exponential backoff (2/4/8/16/32s) for 429 errors.
- **Built-in agents:** `default`, plus the `explore`, `plan`, `verify` and `task`
  sub-agents in `koda-core/agents/`. User and project agent configs override them.

### Key types

- **`KodaAgent`** — Shared resources (tools, system prompt). `Arc`-shareable.
- **`KodaSession`** — Per-conversation state (DB, provider, settings). Has `run_turn()`.
- **`EngineSink`** — Trait with single method: `fn emit(&self, event: EngineEvent)`.
- **`CliSink`** — Channel-forwarding sink. Sends events to the TUI event loop via `UiEvent`.

All providers implement the `LlmProvider` trait (`chat_stream` returning
`Receiver<StreamChunk>`). Tools use PascalCase names.

## Sub-agent approval matrix

`check_tool_for_sub_agent` is context-sensitive (#1251). Sub-agents have no live
human approval channel by design, so what the master matrix would treat as
`⏸ confirm` resolves to **auto-approve for mutations, block for destructive
ops**. The asymmetry pins the bug fix from #1249: before #1251 every Write from
a Safe-trust sub-agent was auto-rejected with "requires user confirmation but
this sub-agent has no channel to the user".

In Auto mode destructive ops still confirm because the user said YOLO for normal
work, not for `rm -rf` (#1251).

## Test tiers in detail

The test-support feature gates `MockProvider` and `TestSink`, which are excluded
from production builds to keep `koda-core`'s public API clean.

**koda-core unit tests** are co-located in `src/` modules, no feature flag:
context tracking, loop detection, config defaults, bash safety, etc.

**koda-core E2E tests** (mock provider, CI) require the `test-support` feature:

- `tests/e2e_test.rs` — core pipeline: text streaming, tool calls, errors, history, cancel
- `tests/e2e_tools_test.rs` — tool coverage: Glob, Grep, Edit, Delete, AST verification
- `tests/e2e_agent_test.rs` — sub-agent invocation + cache hit
- `tests/e2e_skills_test.rs` — skills, compaction, file ownership
- `tests/cancel_test.rs` — Ctrl+C interruption during inference

**koda-core integration tests**, no feature flag:

- `tests/file_tools_test.rs` — path safety, file CRUD
- `tests/new_tools_test.rs` — glob, tool naming
- `tests/guarantee_matrix_test.rs` — trust mode × tool effect matrix
- `tests/inference_recovery_test.rs` — rate-limit retry, context overflow
- `tests/inference_edge_test.rs` — loop detection, empty tool calls
- `tests/e2e_safety_test.rs` — approval gates, bash safety classification
- `tests/session_test.rs` — session lifecycle, TurnStart/TurnEnd events
- `tests/purge_test.rs` — /purge feature (compacted stats, age filter)
- `tests/perf_test.rs` — DB, grep, markdown throughput
- `tests/tool_wiring_test.rs` — every tool routable + approval-handled
- `tests/tool_normalize_test.rs` — snake_case → PascalCase normalization
- `tests/golden_test.rs` — snapshot / golden file tests
- `tests/snapshot_test.rs` — tool definition schema snapshots
- `tests/token_audit.rs` — token estimation accuracy checks

**koda-cli unit tests** are co-located in `src/`: markdown rendering, REPL
parsing, highlighting.

**koda-cli integration tests**, no feature flag:

- `tests/cli_test.rs` — binary subprocess invocation
- `tests/regression_test.rs` — REPL dispatch, input processing
- `tests/server_test.rs` — ACP server integration (JSON-RPC lifecycle)
- `tests/skill_agent_e2e_test.rs` — skill activation + agent switching E2E

**koda-cli smoke tests** (MockProvider, CI-safe): `tests/smoke_test.rs` covers
headless mode: text responses, tool use, session resume, `--resume` flag. The
live variants are gated by `KODA_TEST_LMSTUDIO=1` and never run in CI.

**koda-cli stub-server tests** (`FakeLlmServer`, CI-safe):

- `tests/no_summary_compaction_test.rs` — real headless run with the summary call emptied or failed: the task reaches the next request, the run finishes
- `tests/gemini_stub_test.rs` — real headless run on the Gemini provider: a 503, a tool call and a failed reply in one turn
- `tests/anthropic_stream_failure_test.rs` — an `error` event or a missing `message_stop` discards the reply, and its tool calls don't run
- `tests/history_edit_log_test.rs` — a tool result dropped by a repeated call id shows as a `history edited` warning
- `tests/context_count_test.rs` — a reported token count above the estimate triggers compaction; with no count, the estimate decides
- `tests/headless_interrupt_test.rs` — SIGINT during a model call: the run still reports its turn end and summary
- `tests/stop_reason_log_test.rs` — the `model call` line keeps the stop reason when usage follows the finish chunk

## Release checklist

- Move CHANGELOG.md `[Unreleased]` to a versioned section.
- Bump the version in the three published crates (koda-sandbox, koda-core,
  koda-cli; release.yml checks all three) and in the path-dependency `version =`
  fields that point at them, including koda-test-utils.
- Verify README quick-start examples still work.
- Check that CHANGELOG entries match what's documented in README/DESIGN.md.

## Adding a first-party capability

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
10. Sync version with the workspace
11. Update `CLAUDE.md` and this page

## Toolchain

The toolchain is pinned in `rust-toolchain.toml` (rustup honors it
automatically). CI installs the same version via an explicit `toolchain:` input
on every `dtolnay/rust-toolchain` step, and ci.yml's "Check toolchain pin" step
fails if any workflow drifts from the file. To bump: edit both in one PR and fix
whatever new lints the new toolchain surfaces.

## CI workflows

Six workflows live in `.github/workflows/`:

| Workflow | Trigger | Purpose |
|---|---|---|
| `ci.yml` | PRs to `main`, push to `main` | Gate PRs: format, lint, compile, test, doc, dependency audit |
| `audit.yml` | Weekly (Mon 13:00 UTC) + manual | `cargo audit` of `main`; manages the `security-audit` issue |
| `coverage.yml` | Push to `main`; PRs touching `koda-core/` or `koda-sandbox/` | Coverage thresholds, with auto issue creation on `main` |
| `release.yml` | Tag `v*` | Version verify, cross-platform test + build, publish, Homebrew |
| `docs.yml` | Push to `main` (docs/ changes) | Build + deploy mdBook to GitHub Pages |
| `auto-merge-dependabot.yml` | Dependabot PRs | Enables squash auto-merge; merges once required checks pass |

Release job chain:

```
verify-version → test → build → github-release ─┬→ update-homebrew
                                                 └→ publish (crates.io)
```

crates.io and Homebrew are independent distribution channels: both fan out from
`github-release` in parallel. Publishing to crates.io only after `github-release`
ensures binaries are confirmed good before committing to an immutable version
number; it does not need to wait for Homebrew.

### CI jobs (`ci.yml`)

Six independent jobs run in parallel, one responsibility each, so a red check
names what broke. Wall-clock is set by the slowest (Test).

| Job | Check name(s) | Runs |
|---|---|---|
| `fmt` | `Format` | `cargo fmt --check`, plus text guards: tokio test flavor (#1109), `shell: bash` on the Test step (#1328), toolchain pin |
| `clippy` | `Clippy` | `cargo clippy --workspace --all-targets --features koda-core/test-support -- -D warnings` |
| `check` | `Check (no features)` | `cargo check --workspace --all-targets` (the crates.io shape, no test-support) |
| `test` | `Test (ubuntu-latest)`, `Test (macos-latest)` | `cargo test --workspace --features koda-core/test-support`, then asserts the security-boundary tests ran |
| `doc` | `Docs` | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-Dwarnings` |
| `audit` | `Security Audit` | `cargo audit --deny unsound --deny yanked`, gated on dependency changes (below) |

The pre-push hook (`.githooks/pre-push`) runs only `cargo fmt --check` and
`cargo clippy`, the two cheap checks. CI is the source of truth for the rest.

**Branch ruleset required checks:** `Format`, `Clippy`, `Check (no features)`,
`Test (ubuntu-latest)`, `Test (macos-latest)`, `Docs`, `Security Audit`, i.e.
every `ci.yml` check (verify with `gh api repos/lijunzh/koda/rules/branches/main`).
A required check that never reports blocks merging forever, so **never add a
workflow-level `paths:` filter to `ci.yml`**. Skip work inside a job instead, as
`audit` does.

### Security audit (`ci.yml` `audit` + `audit.yml`)

`cargo audit` goes red for two different reasons: a PR adds a vulnerable or
yanked crate, or RustSec publishes an advisory against a crate `main` already
uses. Only the first is the PR's doing, so the gate is split:

- **PRs:** `Security Audit` runs the audit only when the PR's diff (the
  test-merge commit against its first parent) touches `Cargo.lock`, any
  `Cargo.toml`, or `.cargo/audit.toml`. Otherwise its audit steps are skipped
  and the job still reports success. The check stays required because
  `auto-merge-dependabot.yml` merges every Dependabot PR as soon as required
  checks pass, and a dependency PR must not merge unaudited. The skip is
  step-level on purpose: a job-level `if:` fed by another job would turn a
  failure in that job into a skipped, and therefore passing, required check.
- **Push to `main`:** always audits.
- **Weekly (`audit.yml`):** audits `main` every Monday. On failure it opens a
  `security-audit` issue, or comments on the open one; when the audit passes
  again it closes the issue. A setup failure (e.g. `cargo install` flaking)
  touches no issue. Same fix-forward model and dedup as `coverage-regression`.
  **The `security-audit` label is owned by `audit.yml` alone**; applying it by
  hand breaks the dedup.

### Coverage workflow (`coverage.yml`)

Coverage is **not** a required check. On `main` it tracks the crate thresholds
below and manages issues. PRs touching `koda-core/` or `koda-sandbox/` also run
it for per-file thresholds on security-boundary files (#1265 item 8c), but those
PRs never open or close issues.

| Crate | Threshold |
|---|---|
| `koda-core` | ≥ 80% line coverage |
| `koda-sandbox` | ≥ 80% line coverage |

**Regression handling** — when any crate drops below threshold:

1. Workflow run turns red → GitHub notifies via email.
2. A GitHub issue is automatically created with label `coverage-regression`,
   commit SHA, run link, and which crates failed.
3. If an open `coverage-regression` issue already exists, a comment is added
   instead (no spam). **The label is exclusively owned by `coverage.yml` — do
   not apply it manually or from another workflow, as this breaks deduplication.**
4. When coverage recovers, the open issue is auto-closed with a comment.

**Why not required?** Coverage reruns the full test suite with instrumentation
(2–5× slower). Requiring it would put that cost on every PR's merge path. The
fix-forward model (merge → notify → fix PR) keeps PRs fast while maintaining
accountability on `main`.
