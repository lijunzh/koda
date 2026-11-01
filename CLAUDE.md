# CLAUDE.md

Koda injects this file into its system prompt on every model call, so keep it to
what an agent needs on every turn. Everything else goes in
[docs/src/development.md](docs/src/development.md), which has the full workspace
tree, per-test descriptions, release and new-capability checklists, the toolchain
bump, and how CI, the security audit and coverage work.

## Project Overview

Koda is a personal AI assistant built in Rust (edition 2024) — not a product,
not a platform. See [DESIGN.md](DESIGN.md) for principles (P1: Personal,
P2: Simple enough to own alone, P3: Build for the world six months from now) and
architectural decisions.

**Development philosophy:** Make it work, make it right, make it fast — in that
order. Ship working code first, refactor to clean design second, optimize for
performance only when measured.

### Development docs live in the maintainer's notes, not GitHub issues

Since 2026-10-27, design docs, plans, reviews and decisions are kept in the
maintainer's private notes. On the maintainer's machine they are in
`~/repo/notes/10-projects/Koda/`:
- Start at `Koda.md`: current status, open decisions and the work sequence.
- Then read the `Koda - <Topic>` note for the area you're changing.

That folder is an Obsidian vault with its own `CLAUDE.md`. Follow it when you
write there.

- **Don't open GitHub issues for design discussion, plans or reviews.** Write
  or update the vault note instead.
- **GitHub issues are for** bug reports and feature requests from users, and
  for the `coverage-regression` and `security-audit` issues the workflows manage.
- **Existing issue links stay.** Issues #21–#1391 (March–October 2026) were the
  design record. They remain the history behind the links in code comments,
  DESIGN.md and CHANGELOG.md.
- **The notes are private.** Don't copy them into commits, PRs or public docs;
  state what the change itself needs.

## Commands

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
cargo check --workspace --all-targets                                       # Compile-check all targets, no features (CI enforced)
cargo doc --workspace --no-deps                                             # Build docs
KODA_TEST_LMSTUDIO=1 cargo test -p koda-cli --test smoke_test -- --ignored  # Live test, opt-in, never in CI
```

The toolchain is pinned in `rust-toolchain.toml`; a bump also edits the
workflows. CI runs the checks above; never add a workflow-level `paths:` filter
to `ci.yml`, because a required check that never reports blocks merging forever.

## Architecture

Simple inference loop: stream LLM response → execute tool calls → repeat. No
phases, no tiers — the model drives execution directly.

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` →
`inference_loop()`. The engine talks to clients through `EngineEvent` (output)
and `EngineCommand` (input); approval flows through async channels.

Workspace: `koda-core` (library, pure engine, zero terminal deps), `koda-cli`
(binary `koda`, ratatui TUI), `koda-sandbox` (kernel sandbox runtime),
`koda-test-utils`.

### Where things are

In `koda-core/src/`:
- **Loop:** `inference.rs`, `inference_helpers.rs`, `session.rs` (`KodaSession`),
  `agent/` (`KodaAgent`), `loop_guard.rs`, `turn_context.rs`
- **Prompt and memory:** `prompt.rs`, `instructions.md`, `memory.rs`, `skills.rs`,
  `git.rs` (read-only git context; it never mutates state)
- **Context:** `context.rs`, `compact.rs`, `microcompact.rs`, `model_context.rs`,
  `output_caps.rs`, `truncate.rs`
- **Tools:** `tools/` (`mod.rs` has the registry, dispatcher and
  `safe_resolve_path()`), `tool_dispatch.rs`, `approval_flow.rs`
- **Safety:** `trust.rs`, `bash_safety.rs`, `bash_path_lint.rs`, `sandbox.rs`
- **Sub-agents:** `sub_agent_dispatch.rs`, `child_agent.rs`, `sub_agent_cache.rs`
- **Providers:** `providers/` (all implement the `LlmProvider` trait),
  `provider_catalog.rs`, `model_alias.rs`, `keystore.rs`
- **Storage and protocol:** `db/`, `persistence.rs`, `engine/`, `mcp/`, `config.rs`
- **Undo:** `undo.rs` is an in-memory snapshot stack; `/undo` rolls back the most
  recent turn. It does NOT survive process restarts. There is no on-disk or
  git-backed checkpoint subsystem.

In `koda-cli/src/`:
- **TUI:** `tui_app.rs`, `app.rs`, `tui_context/`, `inference/`, `tui_commands.rs`
  (slash commands), `tui_render.rs`, `tui_viewport.rs`, `scroll_buffer.rs`,
  `composer/` (input box, `SLASH_COMMANDS`), `widgets/`
- **Other frontends:** `headless.rs`, `server.rs` + `acp_adapter.rs` (ACP)
- **Startup:** `main.rs`, `onboarding.rs`, `builtin_skills.rs`

### Approval

Approval is per-tool. A single `TrustMode` enum (Plan/Safe/Auto, `trust.rs`) is
the **single mechanism** for tool gating. It derives kernel sandbox bounds,
per-tool approval and sub-agent defaults from one declaration. There are no
separate "strict mode", "yolo mode" or per-tool toggles.

- **Plan:** only ReadOnly tools; all mutations denied
- **Safe:** local mutations and destructive ops need confirmation
- **Auto:** local mutations auto-approved; **destructive ops still confirm**
- **ToolEffect** (`tools/mod.rs`): ReadOnly / LocalMutation / Destructive / RemoteAction
- **Hardcoded floor:** outside-project writes always need confirmation
- **Sub-agents** have no approval channel, so `⏸ confirm` resolves to
  auto-approve for mutations and block for destructive ops
  (`check_tool_for_sub_agent`). `TrustMode::clamp()` keeps a child from weakening
  its parent.
- **Agent configs** declare `"trust": "plan" | "safe" | "auto"` (`config.rs`).
  Legacy `write_access: bool` still loads with a warning.
- **Folder scoping:** `is_outside_project()` (`trust.rs`), `lint_bash_paths()`
  (`bash_path_lint.rs`), startup warning when project_root == $HOME
- **Process sandbox** (`sandbox.rs`, always on): writes restricted to project +
  /tmp + cache dirs; credential dirs (~/.ssh, ~/.aws, etc.) blocked. macOS uses
  `sandbox-exec -p`, Linux `bwrap`, with a `tracing::warn!` fallback if the
  backend is unavailable.

## Conventions

- Error handling: `anyhow::Result<T>` with `.context()`
- All I/O is async (`tokio`)
- Tool names: PascalCase; module names: snake_case
- `koda-core` has zero terminal deps (no crossterm, no ratatui)
- Single rendering path in koda-cli: `EngineEvent` → `tui_render.rs` →
  `ScrollBuffer` → `draw_viewport()`. No `insert_before()`, no dual-path rendering.
- Engine → client: `EngineSink::emit(EngineEvent)`
- Client → engine: `mpsc::Receiver<EngineCommand>`
- Cancellation: `tokio_util::sync::CancellationToken`
- Cohesion over line count: don't split a file just because it's long. Split
  when pieces have genuinely independent responsibilities. Test: if you must read
  file B every time you read file A, they should be the same file.

### Push and merge

Push after every commit when you can. A task on a branch pushes that branch, not
`main`. Merging a PR and cutting a release need the maintainer's explicit yes.
The pre-push hook (`.githooks/pre-push`) runs `cargo fmt --check` and
`cargo clippy`; CI is the source of truth for the rest.

### Bash tool: pass multi-line / backtick content via files, not inline

The Bash tool runs commands through `sh -c "…"`. Inlining content that contains
newlines, backticks, `$(…)`, or shell metacharacters in the argument string burns
a turn on `sh: -c: line N: syntax error near unexpected token …` (#1232 §7).

**Always-good pattern for `gh` issue/PR creation, commit messages, and any other
multi-line payload:**

```bash
# 1. write the body to a temp file
cat > /tmp/pr_body.md <<'EOF'
## Summary

Multi-line content with `backticks`, $(commands), and "quotes" all
survive because the heredoc is parsed BEFORE sh -c sees the arg.
EOF

# 2. reference it via --body-file / -F
gh pr create --title "…" --body-file /tmp/pr_body.md
git commit -F /tmp/commit_msg.md
```

The heredoc is single-quoted (`<<'EOF'`) so nothing inside it is expanded.

## Documentation Rules

**When to update docs with a PR:**
- User-facing feature added/changed → update root README + relevant crate README
- Architecture or design decision → add to the appropriate section in `DESIGN.md` with rationale
- New crate → must ship with a README.md (required for crates.io)
- Internal refactors don't require doc updates unless they change crate boundaries or public APIs

## Tests

The `test-support` feature gates `MockProvider` and `TestSink`, so E2E tests need it.

- **Unit:** co-located in `src/` modules in both crates.
- **koda-core E2E** (mock provider): `tests/e2e_*.rs` and `tests/cancel_test.rs`,
  using the `Env` harness in `koda-test-utils/src/env.rs`.
- **koda-core integration:** the rest of `koda-core/tests/`, such as
  `guarantee_matrix_test.rs` (trust mode × tool effect), `tool_wiring_test.rs`
  (every tool routable and approval-handled), `snapshot_test.rs` (tool schemas).
- **koda-cli integration:** `tests/cli_test.rs`, `regression_test.rs`,
  `server_test.rs`, `skill_agent_e2e_test.rs`.
- **koda-cli smoke:** `tests/smoke_test.rs`, headless with `MockProvider`; live
  variants need `KODA_TEST_LMSTUDIO=1`.
- **koda-cli stub server:** real headless runs against `FakeLlmServer`, in
  `no_summary_compaction_test.rs`, `gemini_stub_test.rs`,
  `history_edit_log_test.rs`, `context_count_test.rs`,
  `headless_interrupt_test.rs`.
