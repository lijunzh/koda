# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

> **Lineage:** This project continues from [`koda-agent`](https://github.com/lijunzh/koda-agent) (archived at v0.1.5).
> Versions v0.1.0–v0.1.5 of `koda-agent` are documented in that repository's CHANGELOG.

## [Unreleased]

## [0.1.7] - 2026-03-12

### Added
- **Skills system polish** — `/skills` REPL command, `ActivateSkill` tool, E2E tests
  for built-in `code-review` and `security-audit` skills, skills documented in
  system prompt and README (#367)

### Changed
- **Design principles rewritten** — DESIGN.md now states three clear principles:
  Software for One, Clear Boundaries, Make It Work. Removed the old numbered
  decision log in favour of focused architectural guidance (#404)
- **Tool infrastructure simplified** — merged `bash_safety.rs` into `approval.rs`,
  removed `normalize_tool_name()` indirection (tools are always PascalCase),
  collapsed three approval-mode enums into two (Auto/Confirm) (#406, #407)

### Removed
- **Dead code cleanup** — removed `DiscoverTools` tool and trait, `DelegationScope`
  enum, `CreateAgent` placeholder, `model_probe.rs` capability probing, and dead
  git checkpoint/rollback + `FileWatcher` code. ~1,200 lines removed across the
  workspace (#366, #272, #399, #401, #402, #403, #405, #408, #409)

### Fixed
- **Test reliability** — SSE parser tests, dispatch test fixes, session lifecycle
  test with proper shell timeout handling (#384, #385, #386, #398)
- **Parallel tool output** — each tool's banner now appears immediately before its
  own result; previously all banners printed upfront under the first tool's header
  (#410, #411)

## [0.1.6] - 2026-03-11

### Security
- **Keystore TOCTOU fix** — `keys.toml` now created with 0600 permissions atomically
  via `OpenOptions::mode()`, eliminating the window where the file was world-readable
  between `write()` and `set_permissions()` (#387)
- **Gemini API key centralised** — all URL construction goes through `api_url()` helpers,
  removing inline `format!` calls that could leak the key if logged (#389)
- **Proxy credential redaction** — `redact_url_credentials()` strips `user:pass@` from
  all proxy URL log messages (#390)
- **EmailConfig Debug redacted** — custom `Debug` impl shows `[REDACTED]` for password
  field instead of the plaintext value (#391)
- **`.env` in `.gitignore`** — prevents accidental commit of environment files (#392)

### Fixed
- **Removed `unsafe` transmute** in `highlight.rs` — stores `&'static SyntaxReference`
  and creates `HighlightLines` on demand instead of transmuting lifetimes (#388)

## [0.1.5] - 2026-03-11

### Changed
- **Simplified inference loop** — removed phase system, tier system, and OPAR remnants.
  The model now drives execution directly: stream LLM response → execute tool calls → repeat.
  (#354, #355, #357)
- **TUI polish** — removed vestigial tier label from status bar, fixed approval mode
  colors (auto=green, strict=cyan, safe=yellow), model name truncation at 32 chars,
  prompt width uses char count (not byte length), narrow terminal guard (#380)
- **ratatui 0.30** — upgraded from 0.29, migrated tui-textarea 0.7 → ratatui-textarea 0.8,
  crossterm 0.28 → 0.29 (#362)

### Added
- **User guide** — `docs/user-guide.md` covering approval modes, slash commands, file
  references, memory, agents, MCP servers, git checkpointing, headless mode, and
  security model (#299)
- **Capabilities.md refresh** — added `/undo`, `/expand`, `/verbose` commands; approval
  section with mode hotkeys; git checkpointing section; removed stale `/help` (#378)
- **Doc freshness CI gates** — `capabilities_test.rs` verifies slash commands, feature
  keywords, and user guide sections match the codebase (#378)

### Fixed
- **/provider re-prompts for saved API keys** — no longer asks for keys that are
  already stored (#356)
- **Parallel tool display** — concurrent tool executions render correctly (#353, #358)

### Security
- **quinn-proto bumped** 0.11.13 → 0.11.14 — resolves RUSTSEC-2026-0037 (High DoS).
  Not compiled in koda builds (transitive optional dep via reqwest) but flagged by
  cargo audit (#393)

### Documentation
- **DESIGN.md cleanup** — trimmed verbose tables from §14 (viewport), §17 (bash safety),
  §18 (approval) — operational details moved to user guide (#301)
- **Stale docs purge** — removed all phase/tier/agent references from docs, README,
  and code comments (#364, #379)

### Dependencies
- ratatui 0.29 → 0.30 (#362)
- tui-textarea 0.7 → ratatui-textarea 0.8 (#362)
- crossterm 0.28 → 0.29 (#362)
- tree-sitter-go 0.23.4 → 0.25.0 (#359)
- which 7.0.3 → 8.0.2 (#360)
- mail-parser 0.9.4 → 0.11.2 (#361)
- tempfile 3.26.0 → 3.27.0 (#363)
- quinn-proto 0.11.13 → 0.11.14 (#393)

### Testing
- 671 tests across 4 crates (up from 432 in v0.1.4)
- New: doc freshness gates (capabilities commands, feature keywords, user guide sections)

## [0.1.4] - 2026-03-09

### Added
- **Adaptive phase-gated agent loop** (#242) — six-phase state machine:
  Understanding → Planning → Reviewing → Executing → Verifying → Reporting.
  Structural detection via `(current_phase, has_tool_calls, tool_types)` decision tree.
  - `PhaseTracker` with high-water mark, plan approval tracking, review results
  - `TaskIntent`-based initial expectations (file-specificity heuristic)
  - Tier-aware `prompt_hint()` — different guidance per phase per model tier
  - Phase transitions: escalation (Executing → Understanding on tool failure),
    封驳/rejection (Reviewing → Planning on review failure)
- **Phase-aware tool approval** (#242 step 2) — `check_tool()` now consults
  the current phase:
  - Understanding/Planning: writes require confirmation even in Auto mode
  - Executing with approved plan: writes auto-approved
  - Destructive operations: hardcoded floor of NeedsConfirmation regardless of phase
  - `ToolApproval::Notify` variant for de-escalation
- **Phase flow log** (#242 step 3) — `Role::Phase` messages stored in the
  existing messages table. Dual-consumer format: human-readable summary for
  LLM self-awareness + JSON metadata for the InterventionObserver.
  `PhaseTransition` struct with trigger labels (text_only_after_reads,
  simple_task_shortcut, plan_complete, review_passed, 封驳, escalation, etc.)
- **InterventionObserver** (#242 step 4) — per-phase override frequency tracker
  that learns from user behavior. Records auto/override data points at phase
  gates. Autonomy score (0.0–1.0) with configurable threshold. Persists to
  `~/.config/koda/intervention_priors.json`. Cold start defaults to cautious.
- **Folder-scoped permissions** (#218) — three safety layers:
  - Startup warning when `project_root` equals `$HOME`
  - `is_outside_project()`: file tool path args checked against project root
    (hardcoded NeedsConfirmation floor)
  - `lint_bash_paths()`: pre-execution heuristic analysis of bash commands for
    `cd` escapes, absolute paths, and `../` traversals outside project root

### Changed
- **Observe-and-adapt tier system** — all models start at Standard; `TierObserver`
  promotes to Strong after 3 successful tool-use turns, demotes to Lite after
  2+ hallucinated names or malformed args. Name-based tier guessing removed.
- **Context window from API** — `query_and_apply_capabilities()` queries the
  provider API for actual context window and max output tokens. Falls back to
  hardcoded lookup.
- **Decoupled resource limits** — iteration cap (200), parallel tools (always on),
  and auto-compact threshold (85%) are now the same for all tiers.
- **Cloud CLI safe list narrowed** — `gcloud`, `bq`, `aws`, `az` restricted to
  read-only subcommands. Destructive cloud ops now require approval.
- **`sed -i` / `sed --in-place`** added to DANGEROUS_PATTERNS — in-place editing
  via sed is now flagged as destructive.

### Fixed
- **Path scoping key mismatch** — `is_outside_project()` now checks `"path"` key
  (matching actual tool schema) instead of `"file_path"` which never matched.
- **`InterventionObserver::save()`** — logs errors via `tracing::warn` instead
  of silently swallowing write failures.
- **`inference_recovery_test.rs`** — added `required-features = ["test-support"]`
  to Cargo.toml (was breaking bare `cargo test`).

### Refactored
- **`tui_app.rs` god function** (#209) — 1,456-line `run()` split into
  `InputRouter`, `CommandDispatcher`, `ModelSwitcher`, `InferenceRunner`,
  `SessionManager`, and `CompactionManager`. Main function reduced to 66 lines.

### Testing
- 432 tests across 4 crates (up from 489 in v0.1.3 — test consolidation)
- New: 32 phase tracker tests, 10 intervention observer tests, 18 approval
  path-scoping tests, 12 bash path lint tests, 3 integration tests

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
