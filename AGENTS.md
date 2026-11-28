# AGENTS.md

Koda sends this file with every model call, as Codex, Claude Code and most
other agents do, so it holds only what an agent can't see by reading the code.
The workspace tree, where things are, the test tiers, release and
new-capability checklists and how CI works are in
[docs/src/development.md](docs/src/development.md); principles and
architectural decisions are in [DESIGN.md](DESIGN.md). Where this file and the
code disagree, follow the code and say so.

Koda is a personal AI assistant in Rust: not a product, not a platform.

## Development docs live in the maintainer's notes, not GitHub issues

On the maintainer's machine, design docs, plans, reviews and decisions are in
`~/repo/notes/10-projects/Koda/`. Start at `Koda.md`, then read the
`Koda - <Topic>` note for the area you're changing. That folder is an Obsidian
vault with its own `AGENTS.md`; follow it when you write there.

- **Don't open GitHub issues for design discussion, plans or reviews.** Write
  or update the vault note. Issues are for users' bug reports and feature
  requests, and the ones the workflows manage.
- **The notes are private.** Don't copy them into commits, PRs or public docs;
  state what the change itself needs.

## Commands

```bash
cargo nextest run --workspace  # All tests, as CI does (cargo install --locked cargo-nextest)
cargo test --workspace --doc   # Doc tests (nextest skips them; CI runs both)
cargo nextest run -p koda-core -E 'test(/^suite::perf_test::/)'  # One test file (they're modules of tests/all.rs)
cargo fmt --all --check                                # CI enforced
cargo clippy --workspace --all-targets -- -D warnings  # CI enforced
cargo check --workspace --all-targets                  # CI enforced
```

The toolchain is pinned in `rust-toolchain.toml`; a bump also edits the
workflows. Never add a workflow-level `paths:` filter to `ci.yml`, because a
required check that never reports blocks merging forever. A new `ci.yml` job
goes in the `required` job's `needs`, which is what `main` requires. Workflow
changes keep the conventions in `docs/src/development.md` ("Runner hygiene").

## Rules the code doesn't show

- **The client boundary:** `koda-tui` and `koda-cli` (with `koda exec`) reach
  the engine only through the client protocol, as Codex's TUI does. Neither
  may depend on `koda-core`; `scripts/check_tui_core_boundary.sh` (run in CI)
  keeps it out. Add a protocol request or a shared crate instead. Only
  `koda-cli`'s integration tests may use core, to set up state.
- **`koda-core` has zero terminal deps** (no crossterm, no ratatui).
- **One rendering path in `koda-tui`:** `EngineEvent` → `tui_render.rs` →
  `ScrollBuffer` → `draw_viewport()`. No `insert_before()`.
- **`TrustMode` is the single mechanism for tool gating.** Don't add a strict
  mode, a yolo mode or per-tool toggles. Destructive operations confirm even
  in Auto, and writes outside the project always confirm.
- **No file history and no `/undo`.** Git and the shell are how changes come
  back.
- **Git that Koda runs itself** (not the model's, which goes through Bash and
  the sandbox) goes through `koda_git::command` or `tokio_command`, never
  `Command::new("git")`: a repository's config can name programs git runs.
  Diffs add `koda_git::DIFF_ARGS`. Tests may set up repos with plain `git`.
- **Cohesion over line count:** don't split a file just because it's long.
  If you must read file B every time you read file A, they should be one file.

## Push and merge

Push after every commit when you can. A task on a branch pushes that branch,
not `main`. Merging a PR and cutting a release need the maintainer's explicit
yes. The pre-push hook (`.githooks/pre-push`) runs `cargo fmt --check` and
`cargo clippy`; CI is the source of truth for the rest.

## Docs with a PR

- User-facing feature added or changed → root README and the crate's README.
- Architecture or design decision → `DESIGN.md`, with the rationale.
- New crate → ships with a README.md (crates.io requires it).

## Tests

Test code lives in `koda-test-utils` (unpublished), not behind a feature of
the published crates: `MockProvider`, `TestSink` (one file, also compiled by
koda-core's own unit tests) and the stub server, whose `mount_script` plays
the mock's scripts over HTTP for tests that run the `koda` binary or a
sub-agent. One script answers requests in arrival order; agents that call the
model at once (parallel or background sub-agents) each need their own,
`mount_script_for` keyed on how their system prompt begins. The `koda` binary
can't select a mock provider.

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
