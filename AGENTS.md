# AGENTS.md

Koda sends this file with every model call, as Codex, Claude Code and most
other agents do, so keep it to what an agent needs on every turn. Everything
else goes in [docs/src/development.md](docs/src/development.md), which has the
full workspace tree, per-test descriptions, release and new-capability
checklists, the toolchain bump, and how CI, the security audit and coverage
work.

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
cargo nextest run --workspace  # Run all tests, as CI does (cargo install --locked cargo-nextest)
cargo test --workspace --doc   # Doc tests (nextest skips them; CI runs both)
cargo nextest run -p koda-core   # Engine tests only
cargo nextest run -p koda-cli                            # CLI tests only (the pty and exec tests)
cargo nextest run -p koda-tui                            # TUI unit tests
cargo nextest run -p koda-core -E 'test(/^suite::perf_test::/)'  # One koda-core test file (they're modules of tests/all.rs)
cargo nextest run -p koda-cli -E 'test(/^suite::tui_pty_test::/)'  # One koda-cli test file (also modules of tests/all.rs)
cargo fmt --all                          # Format all crates
cargo fmt --all --check                                                    # Check formatting (CI enforced)
cargo clippy --workspace --all-targets -- -D warnings  # Lint (CI enforced)
cargo check --workspace --all-targets                                       # Compile-check all targets, no features (CI enforced)
cargo doc --workspace --no-deps                                             # Build docs
```

The toolchain is pinned in `rust-toolchain.toml`; a bump also edits the
workflows. CI runs the checks above; never add a workflow-level `paths:` filter
to `ci.yml`, because a required check that never reports blocks merging forever.
A new `ci.yml` job goes in the `required` job's `needs`, which is what `main`
requires. Workflow changes keep the conventions in `docs/src/development.md`
("Runner hygiene"): caches saved on `main` only, a timeout per job, actions
pinned by SHA, `persist-credentials: false`.

## Architecture

Simple inference loop: stream LLM response → execute tool calls → repeat. No
phases, no tiers — the model drives execution directly.

`main.rs` → `tui_app.rs` (TUI event loop) → `KodaSession::run_turn()` →
`inference_loop()`. The engine talks to clients through `EngineEvent` (output)
and `EngineCommand` (input); approval flows through async channels.

Workspace: `koda-core` (library, pure engine, zero terminal deps), `koda-cli`
(binary `koda`: the command line, `koda exec`, `koda serve` and `koda mcp`, as
Codex's `codex-cli`), `koda-tui` (the ratatui TUI, as Codex's `codex-tui`),
`koda-oss` (`--oss`: `llama-server` started, shared and stopped with Koda),
`koda-git` (one hardened way to run `git`, as Codex's `codex-git-utils`), `koda-sandbox` (kernel sandbox runtime),
`koda-protocol` (the client protocol's messages and schema), `koda-client`
(the client API: the in-process client, as Codex's `app-server-client`),
`koda-server` (the host and its session tasks, engine events projected onto
the protocol, and `koda serve --stdio`, as Codex's `app-server`),
`koda-config` (the config file `config.toml`: reading, checking and editing it),
`koda-providers` (the model providers and the model tables, which `koda-core`
re-exports as `providers`, `model_alias`, `model_context`, `model_effort` and
`provider_catalog`),
`koda-skills` and `koda-memory` (skills, and the instruction files and Koda's
notes, re-exported by `koda-core` as `skills` and `memory`), `koda-command`
(shell command classification, with `ToolEffect`),
`koda-mcp` (the MCP client, re-exported as `mcp`),
`koda-state` (Koda's stored state: the `SessionStore` trait and its SQLite
`Database`, how its SQLite files are opened, and the diagnostic log database,
as Codex keeps its storage outside `codex-core`), `koda-test-utils`. Outside the
workspace, `apps/desktop` is the desktop app (Electron and a React page, a
client of `koda serve`; not a crate, see "The desktop app" in
`docs/src/development.md`).

**The client boundary:** `koda-tui` and `koda-cli` (with `koda exec`) reach
the engine only through the client protocol, so other TUIs and GUIs can too, as
Codex's CI keeps `codex-core` out of its TUI. Shared crates (`koda-protocol`,
`koda-config`, `koda-state`) are fine; `koda-core` is not a dependency of
either, and `scripts/check_tui_core_boundary.sh` (run in CI) keeps it
out: add a protocol request or a shared crate instead. Only `koda-cli`'s
integration tests may use core, to set up state.

### Where things are

In `koda-core/src/`:
- **Loop:** `inference.rs`, `inference_helpers.rs`, `session.rs` (`KodaSession`),
  `agent/` (`KodaAgent`), `loop_guard.rs`, `turn_context.rs`
- **Prompt and memory:** `prompt.rs`, `instructions.md`, `git.rs` (read-only
  git context; it never mutates state); memory and skills are their own
  crates, `koda-memory` and `koda-skills` (built-in `SKILL.md` files in
  `koda-skills/skills/`)
- **Context:** `context.rs`, `compact.rs`, `microcompact.rs`, `output_caps.rs`,
  `truncate.rs`
- **Tools:** `tools/` (`mod.rs` has the registry, dispatcher and
  `safe_resolve_path()`), `tool_dispatch.rs`, `approval_flow.rs`
- **Safety:** `trust.rs`, `sandbox.rs`; shell command classification in
  `koda-command/src/` (`bash_safety.rs`, `scratch_path.rs`, `ToolEffect`)
- **Sub-agents:** `sub_agent_dispatch.rs`, `child_agent.rs`, `sub_agent_cache.rs`
- **Providers:** `koda-providers/src/` (all implement the `LlmProvider` trait;
  `provider_catalog.rs`, `model_alias.rs`, `model_context.rs`,
  `model_effort.rs`); `keystore.rs` stays in `koda-core`
- **Storage:** `koda-state/src/` (`session_store.rs`, `db/`, `sqlite.rs`,
  `log_db/`), apart from the engine
- **Protocol and config:** `engine/`, `config.rs`; the MCP client is its own
  crate, `koda-mcp`
- **File history:** there is none, and no `/undo`. Git and the shell are how
  changes come back.

In `koda-tui/src/`:
- **TUI:** `tui_app.rs` (entry: `run`), `tui_context/`, `inference/`,
  `tui_commands.rs` (slash commands), `tui_render.rs`, `tui_viewport.rs`,
  `scroll_buffer.rs`, `composer/` (input box, `SLASH_COMMANDS`), `widgets/`
- **Shared with `koda exec`** (public): `input.rs`, `protocol_text.rs`,
  `providers.rs`, `session_id.rs`, `truncate.rs`, `user_config.rs`,
  `retired_env.rs`, `updates.rs`

In `koda-cli/src/`:
- **Command line:** `main.rs`, `app.rs` (arguments and dispatch), `onboarding.rs`
  (the built-in skills are in `koda-server/src/builtin_skills.rs`)
- **Other frontends:** `headless.rs` (`koda exec`), `mcp_cmd.rs` (`koda mcp`)
- **Logging:** `logging.rs`; `--oss` is `koda-oss/src/lib.rs`

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
- **Agent configs** declare `"trust": "plan" | "auto"` (`config.rs`).
  Legacy `write_access: bool` still loads with a warning.
- **Folder scoping:** `is_outside_project()` (`trust.rs`) for Edit and Write,
  with temp dirs exempt (`scratch_path.rs`); a command's paths aren't checked
  (the sandbox holds them, as in Codex); startup warning when project_root == $HOME
- **Process sandbox** (`sandbox.rs`, always on): writes restricted to project +
  /tmp + cache dirs; credential dirs (~/.ssh, ~/.aws, etc.) blocked. macOS uses
  `sandbox-exec -p`, Linux `bwrap`, with a `tracing::warn!` fallback if the
  backend is unavailable.

## Conventions

- Error handling: `anyhow::Result<T>` with `.context()`
- Git that Koda runs itself (not the model's, which goes through Bash and the
  sandbox) goes through `koda_git::command` or `tokio_command`, never
  `Command::new("git")`: a repository's config can name programs git runs
  (hooks, fsmonitor, filters, external diff, textconv). Diffs add
  `koda_git::DIFF_ARGS`. Tests may set up repos with plain `git`.
- All I/O is async (`tokio`)
- Tool names: PascalCase; module names: snake_case
- `koda-core` has zero terminal deps (no crossterm, no ratatui)
- Single rendering path in koda-tui: `EngineEvent` → `tui_render.rs` →
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

Test code lives in `koda-test-utils` (unpublished), not behind a feature of
the published crates: `MockProvider`, `TestSink` (one file, also compiled by
koda-core's own unit tests) and the stub server, whose `mount_script` plays
the mock's scripts over HTTP for tests that run the `koda` binary or a
sub-agent. One script answers requests in arrival order; agents that call the
model at once (parallel or background sub-agents) each need their own,
`mount_script_for` keyed on how their system prompt begins. The `koda` binary
can't select a mock provider.

- **Unit:** co-located in `src/` modules in both crates.
- **koda-core E2E** (mock provider): `tests/suite/e2e_*.rs` and `tests/suite/cancel_test.rs`,
  using the `Env` harness in `koda-test-utils/src/env.rs`.
- **koda-core integration:** the rest of `koda-core/tests/`, such as
  `guarantee_matrix_test.rs` (trust mode × tool effect), `tool_wiring_test.rs`
  (every tool routable and approval-handled), `snapshot_test.rs` (Bash
  classifier verdicts and provider metadata). No test pins tool descriptions.
- **koda-cli integration** (modules of `tests/all.rs`, in `tests/suite/`):
  `cli_test.rs`, `regression_test.rs`,
  `skill_agent_e2e_test.rs`.
- **koda-cli smoke:** `tests/suite/smoke_test.rs`, headless against a `ScriptedServer`.
- **koda-cli stub server:** real headless runs against `FakeLlmServer`, in
  `no_summary_compaction_test.rs`, `gemini_stub_test.rs`,
  `history_edit_log_test.rs`, `context_count_test.rs`,
  `headless_interrupt_test.rs`, `stop_reason_log_test.rs`, and
  `exec_protocol_test.rs` for what `koda exec --json` prints.

### What a test is for

A test checks something Koda does, at the lowest layer that can show it.
These rules come from Codex's and Hermes's, and from the 2026-11 suite review,
which removed about 80 tests that broke them.

- **Not other code:** no tests of `std`, a crate, the mock, or a function
  written inside the test. No tests of static values (a list checked against
  itself, a constant's length), and none that removed logic stays removed.
- **One table per rule,** not one test per literal: a loop over
  `(input, expected)` pairs with the input in the message.
- **Assert the reason, not only the outcome.** An SSRF test that asserts only
  `is_err()` also passes when nothing listens there; check the error says why.
- **A fix's test fails without the fix.** Run it on the old code before you
  commit it.
- **The lowest layer that shows it.** Engine behaviour in `koda-core`, protocol
  in `koda-server`/`koda-client`, rendering and keys in `koda-tui`. A PTY test
  is for what only the running TUI shows (a key or slash command reaching the
  host, terminal modes, signals); its doc comment says which.
- **No fixed sleeps:** wait for the condition (a readiness signal, polling with
  a deadline). No process-wide environment changes: run the test binary again
  with its own environment, as `koda-sandbox/tests/env_scrub.rs` does.
- **No window to fit into.** CI runs eight tests at once on three CPUs, so a
  step can stall for seconds. A reply delayed 3 s, or a command that sleeps
  half a second, is not a place for the test's next step: hold the turn or
  the command until the test lets it go (`FakeLlmServer::gate`,
  `wait_for_go()`, the app's `scriptedModel` gate, a file the command waits
  for, an approval).
- **Wait for the message, don't read what has come.** A notice and the answer
  to a request travel apart, and the notices sent to every connection
  (`session/started`, `session/status/changed` and the rest) reach a client
  that connected a moment later. Checking a notice came: wait for it.
  Checking one didn't: put a later message after it and wait for that.
- **Deadlines:** 30 s on anything that must happen (a hang still fails, by
  nextest's limit). A wall-clock bound that shows "faster than the slow
  path" needs the slow path made far slower than the bound. Never shorten a
  wait, weaken an assertion or add a retry to get a test to pass.
- **A sandbox test that can't run** calls `koda_sandbox::skip_test(why)`, never
  a bare `return`: it fails on CI, where `KODA_REQUIRE_SANDBOX` is set.
