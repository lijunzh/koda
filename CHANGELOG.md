# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Changed (BREAKING for sub-agent dispatch shape)

- **Background-task management trio retired — `WaitTask` / `ListBackgroundTasks` / `CancelTask` removed from the LLM tool surface** (#1325 Phase 5b). The three tools added in #996 Layer 2 are superseded by `WaitForMail` plus the mailbox bridge that landed in #1336: `notify_parent_mailbox` fires from `run_bg_agent` the moment any bg-agent exits, so the parent's mailbox watch-sequence increments before the existing drain-injection path adds the `Role::Tool` row — the model now needs exactly one tool to block on background work, and that tool unifies cleanly with the peer-messaging surface from #1325 Phase 3 (`SendMessage` / `WaitForMail`). Listing in-flight tasks and explicit per-task cancellation never had Codex/Claude-Code/Gemini-CLI equivalents and were a koda-specific surface from the pre-mailbox era — dropping them removes ~1,500 LOC (the 1,397-line `bg_task_tools.rs` minus the ~95 lines of `TaskId` / `parse_task_id` that moved to a new `tools/task_id.rs` module so the TUI's `/cancel <id>` slash command keeps working). The early-dispatch branch in `tool_dispatch::execute_one_tool` that routed the trio around `ToolRegistry::execute()` (it needed `Arc<ChildAgentRegistry>` plus the caller's spawner identity) is also gone, simplifying the dispatch path. `META_TOOLS` in `skill_scope` and `CANONICAL` in `tool_normalize` lost the trio; in their place are the post-#1325 meta tools (`SpawnAgent` / `SendMessage` / `WaitForMail`). The five snake_case aliases (`list_background_tasks`, `list_bg_tasks`, `cancel_task`, `wait_task`, `wait_for_task`) are removed too, so a model emitting any of those names now hits the unknown-tool fallback — the correct signal that the tool is gone. The `InvokeAgent` tool description and module-level rustdoc are rewritten to point at `WaitForMail` instead of `WaitTask` (a regression test inverts the assertion: the description must reference `WaitForMail` AND must NOT reference `WaitTask`, so a future copy-paste can't silently re-introduce the dead pointer). Test migration: `e2e_agent_test::test_sub_agent_cache_hit_skips_llm` switched its barrier between two `InvokeAgent` calls from `WaitTask({"task_ids":["agent:1"]})` to `WaitForMail({})` — the bg-completion mail from #1336's bridge fires AFTER `sub_agent_cache.put`, so `WaitForMail` gives the same happens-before edge that pinned the cache hit before. Test counts: `BUILTIN_TOOLS` 25→22, `tool_wiring_test::HIGHER_LAYER_DISPATCH` is now empty (the bypass list existed exclusively for the bg-task trio). The TUI rendering for historical `WaitTask` / `ListBackgroundTasks` rows in `wait_task_format.rs` (534 lines), `tui_render.rs`, and `history_render.rs` stays put — pre-5b session DBs persisted on disk still contain rows with these tool names and resuming an old session must render them correctly. The underlying registries (`ChildAgentRegistry`, `BgRegistry`, `ChildTaskSnapshot`) are untouched: they still drive the TUI `child_activity_overlay`, the `/agents` and `/cancel` slash commands, and the `notify_parent_mailbox` walk on bg-agent exit — Phase 5b is a tool-surface retirement, not a runtime simplification. DESIGN.md updated: the layer-history of #996 marks Layers 2 and G retired with forward-pointers to the mailbox surface; the "considered and rejected: Codex collab v2" entry rewritten to "considered and selectively adopted" reflecting that #1325 Phases 1–5 actually shipped the mailbox half (Phases 1–2 vendored Codex's `mailbox.rs` substrate, Phase 3 added the peer tools, Phase 4 added per-agent paths and a registry, Phase 5a added `SpawnAgent` plus the completion bridge, Phase 5b is this PR). What's still rejected: `close_agent` / `resume_agent` and persistent ThreadId-addressed agents — koda agents stay session-scoped, no cross-session resume. Net adopted footprint is ~1,500 LOC, half the original 3,000-LOC estimate, because the mailbox machinery is shared between peer messaging and bg-completion delivery. Verified: 1387 `koda-core` lib tests pass (−25 from the deleted `bg_task_tools.rs` test mod), 634 `koda-cli` lib tests pass, all `koda-core` integration suites pass, `cargo clippy --workspace --lib --tests --all-features -- -D warnings` clean.

- **`InvokeAgent` is now spawn-only — the `background:bool` parameter is gone** (#1163, Lean A). Pre-#1163 the model picked between foreground (blocking, returns the sub-agent's final output as the tool result) and background (returns a task_id immediately, result auto-injects on a future iteration) by setting a required `background` boolean on every `InvokeAgent` call. The schema marked it required and the runtime parser rejected missing/wrong-type values, but model compliance with `required` is best-effort and the failure mode of "models silently default to `background:false` and serialize parallel fan-out into blocking calls" was the actual issue that opened #1232: a session showed 10/10 sub-agent calls blocking sequentially even when the model's plan had explicit parallel intent. Lean A collapses dispatch to a single shape that matches Codex's `spawn_agent` and Claude Code's `TaskCreate` — every `InvokeAgent` call spawns a `tokio::spawn`-ed sub-agent and returns its `task_id` synchronously; results auto-drain as user messages on a future parent iteration; `WaitTask([task_id, ...])` is reserved for the rare case where the parent has run out of useful concurrent work AND the next step strictly depends on the sub-agent output. Two semantic fixes fell out of the refactor for free: `SubAgentStart` events now fire on the parent's sink at dispatch time (pre-#1163 the bg path emitted only an `Info { "\u{1f680} ... launched in background" }` line and ACP / headless / e2e clients watching for `SubAgentStart` saw nothing on bg dispatches), and the sub-agent result cache check moved above the spawn block so cache hits short-circuit without spawning a `tokio` task or eating a registry slot. Internals: `execute_sub_agent` gained a private `inline_only:bool` recursion-guard parameter (used by `run_bg_agent` to drive the inference loop inside its own spawned task without re-entering the spawn path); the `parse_background_required` validator and its 7-test mod block were deleted. Test rework: 5 e2e tests in `koda-core/tests/e2e_agent_test.rs` migrated from fg-path-only event surfaces (raw `Info`, `ToolCallResult.output` containing sub-agent text, raw `ToolCallStart{is_sub_agent:true}`) to the post-#1163 surfaces (`AgentStatus::Completed.summary` for sub-agent output, `ChildAgentActivity::ToolStart` for forwarded sub-agent tool counts, `ChildAgentActivity::Info` for forwarded info lines). The `invoke_agent_and_take_calls` test helper now polls for terminal bg status before reading recorded provider calls (pre-#1163 the inline path completed inside `run_inference`, so reads were race-free "by accident"). DESIGN.md, the `child_agent.rs` / `session.rs` / `bg_task_tools.rs` module docs, the `ListBackgroundTasks` model-facing tool description, and the `/agents` TUI command doc all updated for the spawn-only mental model. Total: 1332 lib tests + 12 e2e tests + 634 koda-cli tests green; `cargo clippy --workspace -- -D warnings` clean.

## [0.3.2] - 2026-05-06

The **boring patch release** — every user-visible change is either a bug fix or purely additive. Two Tier-1 security hardenings (#1280 WebFetch SSRF redirect re-validation, #1281 file-mutation symlink/TOCTOU), three real bug fixes (#1264 `/undo` actually undoes, #1266 `gh auth status/list` no longer prompts, #1267 mouse escape sequences no longer leak into the input field), and one additive editor-keymap port from upstream codex (#1278). Internal: completed the 9-PR Tool trait migration (#1265 item 5 — zero user-visible behavior change, but the new ToolCatalog is now THE single source of truth for classification/undo paths/metadata) and a sweeping test-determinism cleanup (#1306, #1308, #1311 — 18 racy `sleep + assert` patterns replaced with deterministic readiness across 5 files). CI gained per-file coverage thresholds for security-boundary modules (#1309) and a fail-closed guard for security-boundary tests (#1307). Workspace tests grew from 2240 to **2594 passing**; `cargo clippy --workspace -- -D warnings` and `RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps` are clean.

### Security

- **File mutation tools: symlink escape + TOCTOU hardening** (#1281, Tier 1 from #1265). Pre-#1281, `safe_resolve_path` was a purely *logical* check (no `canonicalize`) so it caught `../etc/passwd` style escapes but was blind to symlinks: a path like `<project>/sneaky.txt` could be a symlink to `/etc/passwd` (or `~/.aws/credentials`, or `/etc/hosts`), and `Write`/`Edit`/`Delete` would happily clobber the link target. The same bug applied to symlinked parent dirs (`<project>/escape/file.txt` where `<project>/escape -> /etc`). Added `koda_sandbox::fs::verify_mutation_safe` as a single seam: walks up to the deepest existing ancestor, canonicalizes it, and rejects any path that escapes every allowed mutation root. If the target itself exists and is a symlink, the link's canonical target is also re-checked. In-project symlinks (e.g. `examples/latest -> v3/`) still work; only escaping ones are rejected. `LocalFileSystem::write` and `::edit` now use atomic temp-file-then-rename instead of `fs::write`, which closes the residual TOCTOU window: even if a symlink is swapped in between the verifier check and the write, `rename` replaces the symlink itself rather than following it. The same checks apply with `--no-sandbox` (the debug escape hatch is for shell tracing, not for letting the LLM scribble outside the project). `allowed_mutation_roots(project_root)` is the single source of truth shared between `safe_resolve_path` and the verifier so future policy changes land in one place. Total new tests: **14** — 9 unit tests in `koda_sandbox::fs::policy` (escaping final component, escaping parent dir, in-project symlink, race-swap, empty-roots guard, missing-root tolerance) + 5 integration tests in `koda_core::tools::file_tools` (write/edit/delete refuse escaping symlinks via `/etc/hosts`, write allows in-project symlinks). Workspace: 2240 → **2594 tests pass.**

### Security

- **WebFetch SSRF: re-validate every redirect hop** (#1280, Tier 1 from #1265). Pre-#1280, `web_fetch` validated only the initial URL via `is_safe_url` + DNS check, then handed off to a shared `reqwest::Client` whose default redirect policy follows up to 10 hops with NO re-validation. A public-looking URL could redirect to `127.0.0.1`, `169.254.169.254` (cloud metadata), or any RFC1918 private host, and reqwest would silently follow. WebFetch now uses its own client with `redirect::Policy::none()` and a hand-rolled `safely_follow_redirects` loop that re-runs the full SSRF check (host blocklist + IP-range check + DNS pre-check) on every hop, including relative and scheme-relative `Location` headers. The 15s timeout now bounds the entire chain (initial + all redirects), not just the first request. The shared `build_http_client` is unchanged and still defaults to reqwest's max-10-hops policy — the new `build_http_client_with_redirect_policy` is opt-in. Six new redirect re-validation tests cover: redirect to loopback blocked, redirect to cloud-metadata blocked, max-hop limit enforced, relative `Location` resolves correctly, scheme-relative `Location` re-validated, and a happy-path two-hop chain. Total web_fetch test count: 14 → 20.

### Added

- **Modified Backspace/Delete defaults in vendored editor keymap** (#1278). Ported upstream codex commit `87d2235b54` (#21058) into `composer/keymap.rs` and `composer/textarea.rs`: the default `EditorKeymap` now binds `Shift+Backspace` and `Shift+Delete` to grapheme-delete (so Windows terminals that distinguish modified delete keys don't drop them on the floor), plus `Ctrl+Backspace`, `Ctrl+Shift+Backspace`, `Ctrl+Delete`, and `Ctrl+Shift+Delete` to word-delete (matching Windows text-input conventions). 4 new regression tests pin the new defaults so future refactors of `RuntimeKeymap::defaults()` can't silently drop them.
- **Vendor-sync skip annotations + workflow filter** (#1278). The vendor-sync workflow now reads `//! - skip <SHA>: <reason>` lines from each vendored file's `## Vendor-sync skips` doc-comment section and excludes those upstream commits from the drift report. Lets us record "reviewed and intentionally NOT porting" decisions per-commit without having the workflow nag about them every Monday. Skipped commits are surfaced in their own table in the issue body so we can periodically reconsider. The issue body's "How to act on this" section was rewritten to push reviewers toward fit evaluation ("does this commit fit koda's needs?") instead of blind syncing. As of this PR, 3 distinct upstream commits are skipped: `48402be6fa` (TUI keymap coverage — needs `/keymap` UI we don't have), `94800ecbbf` (keymap debug inspector — same), and `36912ce3de` (Windows paste burst interval — awaits Windows test target).

- **`create_provider` routing tests** (#1264 priority 9). The function `koda_core::providers::create_provider` is the central provider-construction switchboard — it's called from at least 8 sites (`session::update_provider`, `tui_commands` for `/model`, `tui_context::menus` for the provider picker, `headless`, `server`, `sub_agent_dispatch`, `tui_context` session bootstrap, `builtin_proxy` smoke check). Despite that fan-in, it had **zero direct routing tests** — a `match` arm regression here would silently misroute a whole provider family at runtime, surfacing as "my Anthropic key doesn't work" reports that are actually "the request went to OpenAI-compat with the wrong base URL." Added 4 tests pinning every current `ProviderType` variant explicitly: `Anthropic` → `"anthropic"`, `Gemini` → `"gemini"`, `Mock` → `"mock"`, and the OpenAI-compat fallthrough family (`OpenAI`, `Groq`, `DeepSeek`, `Fireworks`) → `"openai-compat"`. The fall-through arm `_ => Box::new(openai_compat::...)` is the specific footgun: any new `ProviderType` variant added to the enum without a matching arm gets silently routed to OpenAI-compat (Rust doesn't warn on `_` arms catching new variants — by design — so we backstop with a test that lists the family explicitly and tells future contributors how to extend it). Closes #1264 priority 9.

### Fixed

- **Internal `koda-core` dep version drift in `koda-cli/Cargo.toml`** (#1265 Tier 2). Runtime path-dep was pinned to `0.3.1` but the dev-dep (`features = ["test-support"]`) was still on `0.3.0`, which would fail to build the moment `koda-core` ever published as a real registry crate. Bumped both to `0.3.1`. Trivial, folded into the WebFetch SSRF PR for hygiene.

- **System prompt no longer lies about a non-existent git-checkpointing feature** (#1264 priority 2). The system prompt sent to every LLM call contained:

  > ### Git Checkpointing
  >
  > Auto-snapshots working tree before each turn. `/undo` to rollback.

  No such feature exists. `git.rs` is *read-only* — it runs `git rev-parse`, `git diff --stat`, and `git log` to format a context block for the system prompt; it never mutates state. The module's own doc comment even says so under "What this module does NOT do". The actual `/undo` is the in-memory stack in `undo.rs` (which itself was completely broken until #1271 yesterday) and dies with the process. Models receiving the false claim were reassuring users that destructive operations were safe to attempt because "the auto-snapshot will catch it" — they weren't, and it didn't. Replaced with a truthful Undo section that explicitly calls out the durability gap and tells the model to advise users to commit before quitting if they want durable rollback. Also fixed two stale references in `CLAUDE.md` (architecture overview line + module map line) that pointed at the same phantom feature. Added `prompt::tests::prompt_does_not_lie_about_git_checkpointing` regression guard so the marketing copy can't sneak back in via copy-paste from old git history. Closes #1264 priority 2 with a "drop the scaffolding" rationale; if a real git-backed checkpoint subsystem ever lands, delete the regression test and update the prompt with the truth.

### Added

- **Context-window management — 4 new e2e scenarios** (#1264 priority 4). The existing `inference_context_test.rs` covered display accuracy only (the `ContextUsage`/`Footer` event emission, see #874 / #946). New `koda-core/tests/context_management_e2e_test.rs` covers the *behaviour* gaps the issue called out: pre-flight compaction firing when the heuristic gauge crosses [`AUTO_COMPACT_THRESHOLD`] (85%); pre-flight compaction being skipped under the threshold; provider-rejected context overflow triggering `try_overflow_recovery` → compact → retry → success; and overflow recovery propagating the original error when the session is too short for compaction to help (`CompactSkip::TooShort`). Module doc explains the `MockResponse` ordering required for the compact summary call vs. the streamed chat call. Tests are serialized via a file-scoped `LazyLock<Mutex>` because `compact::CONSECUTIVE_FAILURES` is a process-global atomic — same hazard the existing `compact.rs` test module called out at line 723 when it deleted duplicate breaker tests.

- **ACP server protocol coverage — 8 new e2e scenarios** (#1264 priority 1). The ACP stdio server previously had a single `test_server_protocol_smoke` test covering `initialize` → `session/new` → `cancel` → `initialize`. Expanded `koda-cli/tests/server_test.rs` to a 10-scenario `test_server_protocol_e2e` test — still one subprocess (the original spawn-flake mitigation, see module docs), but now with named `step_*` helpers for each scenario so failure attribution stays readable. New scenarios: unknown-method returns `-32601`; `session/prompt` without an active session returns `-32000` with helpful message; `session/cancel` notification without an active session is a no-op; `authenticate` no-op responds successfully; second `session/new` returns a distinct id (documents the single-slot `state.active` contract); malformed JSON line returns `-32700` with `id: null` and the server stays alive; final post-recovery `initialize` confirms the server survived everything.

### Fixed

- **`/undo` now actually undoes** (#1264 priority 3). The undo stack's `commit_turn()` was never called from production code — only from `undo.rs`'s own unit tests. The tool dispatcher correctly pushed per-file snapshots into `pending` before each Write/Edit/Delete, but those snapshots never became an undoable entry, so `/undo` always reported "Nothing to undo" even after the agent had rewritten files. Fixed by wrapping `inference_loop` in a thin shim that always calls `tools.undo.commit_turn()` on return regardless of outcome (Complete / Cancelled / Error) — `commit_turn` is a no-op when `pending` is empty, so turns with zero mutations don't grow the stack. Added 8 E2E tests in `koda-core/tests/undo_e2e_test.rs` covering: Write-overwrite restore, Write-create rollback (file removed), Edit restore, Delete restore, multiple sequential mutations in one turn → 1 entry, parallel ToolCalls in one response → 1 entry, two turns → 2 independent entries with LIFO ordering, read-only tools (Glob/Grep/Read) → 0 entries. Also corrected the stale doc comment in `undo.rs` referencing a non-existent "git checkpointing" subsystem.

- **Mouse escape sequences (e.g. `[<65;107;38M`) no longer leak into the input field** (#1267). `init_terminal` was using `crossterm::event::EnableMouseCapture`, which enables five mouse-tracking modes including `?1003h` (any-event tracking — fires on every pixel of motion). Koda only consumes scroll wheel + left-click drag, so the extra event volume was pure overhead — and under heavy scroll/move flow the read buffer could fragment between the leading `\x1b` byte and the `[<…M` body, causing crossterm's parser to emit ESC alone and then each printable char individually. Replaced with a custom `EnableSelectiveMouseCapture` command that emits only `?1002h` (button-event tracking) + `?1006h` (SGR coordinates), matching the mode set used by gemini-cli. Pinned the exact byte sequences in three new unit tests (`mouse_capture_tests`).
- **`gh auth status` and `gh auth list` no longer trigger an approval prompt in `Auto` mode** (#1266). `bash_safety::DANGER_CHECKS` previously matched the entire `gh auth` family with a single `CmdSub("gh", "auth")` rule, so read-only auth queries were classified as `Destructive` alongside the credential-mutating subcommands. Split the rule into the specific destructive subcommands (`login`, `logout`, `refresh`, `setup-git`, `token`) and added the read-only forms (`gh auth status`, `gh auth list`) to `READ_ONLY_PREFIXES`. Same approach the file already uses for `gh issue` and `gh pr`.

## [0.3.1] - 2026-05-05

### Fixed

- **Persisted/resumed `Auto` sessions now obey the same sandbox gate as startup** (#1241 release audit). Fresh CLI/env/default startup already rejected Auto when the kernel sandbox was unavailable, but session restore paths trusted the DB mode directly. A session persisted as `auto` on a sandboxed machine could therefore be resumed on an unsandboxed host and bypass the headline invariant. TUI startup and `/sessions resume` now route persisted modes through the same top-level resolver: parse → Plan-to-Safe coercion → `require_sandbox_for_auto`. Invalid persisted Auto fails loudly with the same setup hint instead of silently entering Auto.

### Changed (BREAKING for first-run UX)

- **Default trust mode flipped from `Safe` → `Auto`** (#1241). Running `koda` with no `--mode` flag and no `KODA_MODE` env var now starts in Auto mode (auto-approve mutations within the kernel sandbox; destructive ops and outside-project writes still confirm). The previous Safe-by-default — which prompted on **every** mutation — was the dominant source of "why is this thing nagging me?" friction and effectively encouraged users to flip to Auto on first use, making the safety value of Safe-by-default mostly theatrical.
  - **Why this is safe to flip now**: the kernel sandbox (#1259), outside-project floor (`is_outside_project`), destructive backstop (#1251 — `Auto × Destructive` confirms, doesn't auto-approve), scratch-zone allowlist (#1236), and pre-flight context-budget check (#1237) are all in place. The trust badge is also legible (#1257) and accompanied by a sandbox indicator (#1259) so the active mode is unmissable.
  - **What happens on unsandboxed platforms (Linux without bwrap, Windows)**: Auto refuses to start with an actionable error that includes platform-specific install instructions (#1259 + #1260). Users either install the sandbox backend (one command) or pass `--mode safe` to opt into the previous behavior. We picked **loud-fail-fast** over silent coercion for the same reason #1259 did: silent coercion in headless (`koda --mode auto -p "..."` becoming Safe and aborting every mutation) is catastrophic.
  - **Migration for users who want the old behavior**: pass `--mode safe`, set `KODA_MODE=safe` in your shell, or per-session toggle with `Shift+Tab` in the TUI.
  - **Migration for CI users**: explicitly pass `--mode safe` in your CI command. CI shouldn't rely on default behavior anyway.
  - **No silent change for sub-agents**: per-agent `trust:` declarations in agent JSON are unaffected. Built-in agents continue to ship with their explicit `trust: "safe"` / `trust: "plan"` declarations. Only the **top-level session default** changes.

### Documentation

- **Trust-mode mental model documented end-to-end across all surfaces** (#1250 doc follow-up). Updated the user-facing book in `docs/src/`, the architecture overview in `DESIGN.md`, and the contributor primer in `CLAUDE.md` to reflect the post-#1251/#1252 state:
  - **`docs/src/approval.md` rewritten as the canonical mental-model doc**: opens with a one-paragraph north star ("trust is the single mechanism; sandbox is the always-on floor"), the three modes with badge previews, the **corrected** top-level matrix (`Auto × Destructive` now ⏸ confirm — was ✅ auto pre-#1251), the **new sub-agent context-sensitive matrix** with the safe-side-rule rationale (mutating ops auto-approve, destructive ops block, since sub-agents have no human channel), the always-on safety floors (kernel sandbox, outside-project floor, sandbox-unavailable downgrade #860, agent-file protection, credential scrub #1228), approval keys, the per-agent `trust` field, and headless-mode behavior.
  - **`docs/src/agents.md` learns the `trust` field**: added to the field table with one-line description, three example shapes (read-only investigator / write-capable worker / read-only-with-execution escape valve), full `write_access` migration table, complete built-ins table (`default`, `task`, `explore`, `plan`, `verify` — was just `default` + nonexistent `guide`).
  - **`docs/src/tools.md` matrix corrected**: "Destructive shell" row now `Auto: ⏸ Prompt` (was `✅ Auto`); leading paragraph mentions destructive ops still prompt in Auto; pointer to the sub-agent matrix added.
  - **`docs/src/sandbox.md` sub-agent section updated**: replaces the stale `"mode": "auto"` JSON example with `"trust": "auto"`, points to the new sub-agent matrix section in `approval.md`.
  - **`DESIGN.md §Security Model` rewritten**: adds the "TrustMode is the single mechanism" framing tying the design back to P1 ("customization over configuration"), documents the `Auto × Destructive` tightening, the sub-agent context-sensitive matrix, the per-agent `trust:` declaration, and the env scrub (#1228). Cross-links to `docs/src/approval.md` as the canonical user-facing reference.
  - **`CLAUDE.md §Approval` corrected**: line previously said "Auto: everything auto-approved; sandbox enforces the perimeter" — wrong post-#1251. New text: "Auto: local mutations auto-approved; destructive ops still confirm." Adds the single-mechanism framing, the sub-agent matrix bullet, the per-agent `trust:` field bullet, and the bug-fix reference to #1249.

- **CLAUDE.md teaches the `gh --body-file` heredoc pattern for multi-line content** (#1232 §7). The Bash tool runs commands through `sh -c "…"`; inlining content with newlines, backticks, `$(…)`, or quotes burns a turn on `sh: -c: line N: syntax error near unexpected token …`. The bug-review session that opened #1232 reproduced this exact failure when the model called `gh issue create --title "…" --body "…"` with a markdown body containing backticks. New "Bash tool: pass multi-line / backtick content via files, not inline" subsection in `CLAUDE.md §Conventions` shows the always-good heredoc-into-temp-file pattern (`<<'EOF'` is single-quoted so nothing inside expands) and lists the canonical sites: `gh pr create --body-file`, `gh issue create --body-file`, `git commit -F`. CLAUDE.md is loaded as project memory on every session, so the nudge reaches both the master agent and every sub-agent. No code changes — docs only.

### Behaviour change

- **Sub-agent trust matrix is now context-sensitive; `write_access` is deprecated in favor of `trust`** (#1250, #1251 PR A, this PR is PR B). Two changes that close the dead-channel bug from #1249 and consolidate the agent-scoping mechanism:

  **Trust matrix (#1251)**: `koda_core::trust::check_tool` gains two new sub-agent variants — `check_tool_for_sub_agent` and `check_tool_for_sub_agent_with_tracker`. They resolve `NeedsConfirmation` via the safe-side rule: mutating ops (Write/Edit/MemoryWrite) auto-approve, destructive ops (`rm -rf`, `git reset --hard`, `git push --force`, Delete) are blocked. This fixes the production bug where every Write from a Safe-trust sub-agent was auto-rejected with *"requires user confirmation but this sub-agent has no channel to the user"* — because sub-agents have no live human approval channel by design (#1022 B10). The same PR also tightens the top-level matrix: `Auto × Destructive` now requires confirmation (was: auto-approved within sandbox). The user said YOLO for normal work, not for `rm -rf`.

  **Agent JSON migration (this PR)**: built-in agents migrate from `write_access: bool` to `trust: "plan" | "safe" | "auto"`. The new field is the single mechanism — the trust matrix derives everything from it (kernel sandbox bounds, per-tool approval rules, sub-agent context-sensitive defaults). `write_access: true` in user JSONs still works but emits a deprecation warning at load. Built-ins shipped: `default → trust:safe`, `task → trust:safe`, `verify → trust:safe + disallowed:[Write,Edit,Delete]` (read-only-with-execution escape valve), `explore`/`plan` keep `trust:plan`. The `disallowed_tools` field is also pruned from built-ins where the trust matrix already gates the tool — what remains is the behavioral floor (`InvokeAgent`, `AskUser`, `TodoWrite`) for tools classified as `ReadOnly` that the matrix can't gate.

  **Migration**: drop `write_access` from your custom agent JSONs and add `trust: "safe"` (write-capable) or `trust: "plan"` (read-only). Pre-#1250 JSONs without `trust` continue to work via legacy default-deny (Write/Edit/Delete injected into `disallowed_tools` when `write_access` is false or absent). The `create-agent` skill is updated to teach the new mechanism. End-to-end pin in `koda-core/tests/e2e_sub_agent_trust_test.rs` proves a Safe-trust sub-agent now writes files successfully while still blocking destructive Bash.

### Security

- **Sandboxed shell tool calls now scrub the parent process env down to a fixed allowlist** (#1228). Before this change, every `bash -c '…'` style tool call dispatched by the LLM inherited the entire koda parent env, including `OPENAI_API_KEY`, `AWS_SECRET_ACCESS_KEY`, `GITHUB_TOKEN`, and any other secrets the user happened to have exported. A prompt-injected sub-agent that convinced the model to run `env` or `printenv` would exfiltrate them straight into the LLM transcript (and from there into the next provider request). The new `koda-sandbox::env::scrub` is called from all three sandbox runtimes (`seatbelt`, `bwrap`, `UnsandboxedRuntime`) and applies a two-layer allowlist: a fixed base set (locale, identity, `PATH`, tmpdir, proxy URLs) plus per-tool extras keyed on the resolved binary name (`cargo` gets `CARGO_HOME`, `git` gets `GIT_AUTHOR_*`, `aws` gets `AWS_PROFILE` but **never** `AWS_SECRET_ACCESS_KEY`, etc.). Allowlist is `pub const` for security audit. **Migration:** if a tool you rely on needs a custom env var (e.g. `MY_PROJECT_VAR=foo`), set it inline in the command (`MY_PROJECT_VAR=foo cargo build`), or **edit `koda-sandbox/src/env.rs`** — add to `SAFE_BASE_VARS` for unconditional, or to the appropriate `tool_extras_for` arm for tool-specific — and rebuild. Per DESIGN.md P1 ("customization over configuration"), there will be no runtime config knob to override the allowlist; every change to the security boundary lives in source so it gets PR-reviewed and ships in a release users can pin. The proxy/stage2 control vars set by koda's own infrastructure (`STAGE2_*`, `KODA_SANDBOX_PROXY_PORT_DEBUG`) are unaffected because they're written *after* the scrub.

### Behaviour change

- **Public engine enums are now `#[non_exhaustive]`** (#1224 — closes the v0.3.0 P3 bundle). `EngineEvent`, `BgChildActivityKind`, `TurnEndReason`, and `AgentStatus` all gain the marker, which means downstream `match` consumers must add a `_ =>` arm. This is a one-time breaking change — v0.3.0 already burned a minor bump for the `EngineEvent::BgChildActivity` addition (#1206), so we're cashing in on the same minor-version-window to install the marker before the next variant addition forces *another* breaking change. After this lands, future variant additions to these four enums will be additive (non-breaking) for any downstream that handles the wildcard arm. In-tree consumers (`koda-cli/src/{acp_adapter, bg_activity, headless, tui_render}.rs`) gain nine semantically-correct fallback arms (drop-silent for ACP, generic-render for headless/TUI) — audited individually rather than blanket `_ => ()`.

### Internal

- **Composer cursor wiring regression test** (#1224 item 2). Two `TestBackend`-backed contract tests in `tui_viewport::cursor_wiring_tests` assert that `TextArea::cursor_pos_with_state` produces caret coordinates AND that `Frame::set_cursor_position` actually moves the backend cursor. Catches a future regression where someone deletes the cursor-parking block from `draw_viewport` (#1220/#1221).
- **Stale Windows-only ignored test gated at compile-time** (#1224 item 3). `composer::textarea::tests::altgr_ctrl_alt_char_inserts_literal` was using `#[cfg_attr(not(windows), ignore)]`, which made the test perpetually appear in the `1 ignored` count for every macOS/Linux contributor. Switched to `#[cfg(windows)]` so the test only compiles where it can actually run; the `1 ignored` line is gone from `cargo test -p koda-cli --lib` output.

## [0.3.0] - 2026-05-03

**The bg-agent observability release.** v0.3.0 converges on “user always knows what's happening, and can always interrupt it.” Headlines: an always-on **bg-activity overlay** above the status bar (no slash command needed), a new **`BgChildActivity` engine event** that powers it (and surfaces in ACP / headless), an **Esc/Ctrl+C cancel cascade overhaul** so a single Esc reliably interrupts the master AND every bg sub-agent it spawned, and a **slash-command guard during inference** that stops `/cancel agent:1` from being silently re-routed to the model as a steer. The legacy modal `/agents` panel and the `/agents` slash command are gone—superseded by the always-on overlay. Behavioural changes are called out below.

### Behaviour change

- **`EngineEvent` enum gains a new `BgChildActivity` variant** (#1206). This is a **breaking change for downstream `match` consumers** (the enum is not `#[non_exhaustive]`)—the addition is the explicit justification for the `0.2.x → 0.3.0` minor bump per Cargo's pre-1.0 semver convention. Action for embedders: add the new arm or a `_ =>` wildcard. Future variant additions will be considered for `#[non_exhaustive]` adoption (deferred from this release per release-time audit—adding the marker now would force 9 internal `_` arms with subtle behavior implications, better as its own PR).
- **`/agents` slash command and the interactive `/agents` panel are removed** (#1210, supersedes #1191). Replaced by the always-on bg-activity overlay; `ListBackgroundTasks` (LLM-facing) and `/cancel <id>` (user-facing) cover the remaining surface. Net deletion: −1132 LoC. See `docs/src/commands.md` `/agents` section for migration notes.
- **Esc and Ctrl+C semantics formalised** (#1217). Both still cancel the current inference, but the cancel now **cascades through every bg sub-agent the master spawned** (was: only the master returned, bg agents kept running silently). Idle Esc clears the composer or interrupts bg work; idle Ctrl+C is a two-press exit (first arms, second within 1500ms exits). See `docs/src/keybindings.md` and `docs/src/tui.md`.

### Added

- **Always-on bg-activity overlay above the status bar** (#1213, supersedes #1191). Renders one row per running bg sub-agent or shell process with `🤖 name (age) · last activity`, capped at a configurable visible count with `+ N more` overflow, and a context footer line (`Esc cancel all  ·  /cancel <id>`). Updates live from `BgChildActivity` events without writing to scrollback. Decoupled from `koda-core` types—the widget takes pre-formatted rows (29 widget tests, zero engine setup).
- **`EngineEvent::BgChildActivity { task_id, spawner, kind }` event variant** (#1206). The variant carries one of three `BgChildActivityKind` values—`ToolStart { tool_name, summary }`, `ToolEnd { tool_name, success }`, `Info { message }`—emitted from inside child tool dispatch via the new `ForwardingBgSink`. Powers the overlay, surfaces in the ACP adapter, and renders sanely in headless mode. Includes 4 roundtrip tests in `engine/event.rs` and 6 fan-out tests in `engine/sink.rs`.
- **Paste-burst suppression wired into the idle event loop** (Phase A of #1186, #1189). Multi-character paste bursts now coalesce in the composer rather than firing per-keystroke handlers, eliminating the visible “type-streaming-into-the-input” effect on large pastes.

### Changed

- **Esc/Ctrl+C cancel cascade overhaul** (#1217, closes #1216). Introduces `SessionCancel`—an `Arc<RwLock<CancellationToken>>` cloneable handle—that decouples the cancel root from the `&mut session` borrow held by `run_turn`. The token cascades through `tokio_util::sync::CancellationToken::child_token()` to every bg sub-agent registered under the master, so a single Esc unwinds the whole tree. Pinned by `cancel_handle_tests` (cascade, swap, Send+Sync clone, unblock-within-ms) plus `execute_wait_unblocks_immediately_on_master_cancel` in `bg_task_tools`. The original symptom: pressing Esc during a `WaitTask` returned the master cleanly but left bg agents running until they hit their own deadlines—often minutes later, with no UI signal.
- **Banner + footer density restructure** (#1197, closes #1194 + #1195). The TUI banner is now a single line (was three) and the status bar / key-hint footer renders denser; reclaims ~3 lines of vertical real estate on standard terminals.
- **`WaitTask` tool description now discourages immediate-wait-after-spawn** (#1205, A1 of #1201). The tool docstring nudges the model toward fire-and-iterate (spawn bg, continue working, let the result auto-inject as a `Role::Tool` message on a future iteration—see #1159) rather than spawn-then-wait, which collapses the parallelism gain. Soft fix at the prompt layer; no behavioral change.
- **`ListBackgroundTasks` tool result renders as a per-task summary table** (#1215, closes #1209) instead of a JSON blob in transcripts and live TUI. Mirrors the #1162 `WaitTask` pretty-printer; raw JSON falls back through on any parse failure so we never lose content.
- **`/cancel <id>` flips the overlay icon red immediately** (#1210), before the registry observes the cancellation—was: a few hundred ms of silent staring while the inference loop noticed the cancel token. The same instant feedback applies to global Esc/Ctrl+C cancel-all (#1200/#1202).

### Removed

- **Interactive `/agents` panel and `/agents` slash command** (#1210, supersedes #1191). See **Behaviour change** above.
- **Per-event `BgChildActivity` scrollback render** (#1207, B2 of #1201). The earlier B-phase prototype (#1206) wrote each child activity event into the durable scrollback—50 dim lines per 50-tool agent. Now the always-on overlay (#1213) covers live signal, and only the post-completion `✅ … completed` Info line is committed to scrollback as the durable record.

### Fixed

- **Slash commands typed during inference no longer silently steer the model** (#1211, #1222). Inference Enter (`QueueNext`) and Ctrl+J (“later”) handlers in `tui_handlers_inference.rs` sent textarea contents straight to the engine without checking for a leading `/`. So `/cancel agent:1` typed during a `WaitTask` would arrive at the model as a raw user message—the bg agent kept running, the user got no feedback, and `📥 Next: /cancel agent:1` would render in scrollback as if it were a real steer. Both paths now consult `is_slash_command_attempt` (4 pinned classifier tests in `slash_guard_tests`) and push a visible warning instead of queueing.
- **Composer now shows a visible cursor at the current input position** (#1220, #1221). The textarea exposed a `cursor_pos(area)` API matching codex's `Renderable::cursor_pos`, but `tui_viewport::draw_viewport` never called `Frame::set_cursor_position`—ratatui's default cursor-hidden state stuck and users had no visual indicator. Worse: **CJK/IME preedit was broken** (composition rendered at screen origin, not caret) and **screen readers / magnifiers couldn't track input position** (WCAG 2.1.1 / 2.4.7). Fix mirrors codex: after rendering the textarea, call `frame.set_cursor_position((x, y))` with screen coords from `cursor_pos_with_state`. Cursor *style* (vim block vs. bar) is intentionally a follow-up.
- **`TodoWrite` no longer requires approval and is no longer blocked in Plan mode** (#1212, #1218). The tool was misclassified as `LocalMutation` alongside `Write`/`Edit`/`MemoryWrite`—`koda-core::trust` gated it as `NeedsConfirmation` in Safe and `Blocked` in Plan, breaking the very tool that's supposed to be the canonical planning surface. `TodoWrite` only mutates Koda-owned session state (the in-memory todo list), not the user's files, so it's now classified as `ReadOnly` and auto-approves in every trust mode. Tests in `trust.rs` + `tool_wiring_test.rs`, plus the `tools/mod.rs` doc table and `docs/src/tools.md`, were updated to match.
- **Per-turn `CancellationToken` now threads through `run_turn`** (#1208, #1214). The token used to be created at session-start and reused across turns, which meant a cancelled turn left a tripped token that immediately cancelled the next inference. Now each turn gets a fresh child token via `SessionCancel::child_for_turn()`, with the cascade still rooted at the session token from #1217. Pinned by `session_turn_cancel_token_stops_inference`.
- **Bg-agent completion is now persisted as `Role::Tool` keyed on the parent's `tool_call_id`** (#1159, #1193). Was: completion landed as a free-form assistant message disconnected from the originating `InvokeAgent { background: true }` call, so a follow-up “what was the result?” query couldn't find it. Now it's a proper `Role::Tool` message attached to the parent tool call, matching the foreground InvokeAgent path. +236 LoC of `db/tests.rs` coverage.
- **Ctrl+C cancel cascade for background sub-agents** (#1200, #1202). Pre-#1217 prep: ensures the master's Ctrl+C handler propagates to every bg sub-agent registered under it. Subsumed by the broader cascade overhaul in #1217 but worth its own line because it shipped first and was the reproducer for the larger fix.
- **Flaky tests in `koda_core::context`** (#1203, #1204). Two tests intermittently failed under `current_thread` runtime under macOS CI load. Switched to `multi_thread, worker_threads = 2` (matches production); 5 consecutive clean local runs documented in commit body.
- **`tui-context` / events module test merge-skew** (#1192). Restored `use super::*` in two `mod tests` blocks dropped during the parallel landings of #1189 and #1190. CI-only regression, fixed forward.

### Performance

- **Per-session context cache for `load_context`** (#1196, closes #1166 audit item A). The session-bound context assembly (sandbox config, project root, allowlist, mcp registry, etc.) used to be re-loaded from SQLite on every turn; now the per-session cache (new `db/context_cache.rs`, +203 LoC) memoizes it and invalidates surgically via `Database::clear_context_cache_for(session_id)` when the underlying rows change. **93–96% reduction** in `assemble_context` wall time per `assemble_context_bench.rs`.

### Internal

- **WaitTask + ListBackgroundTasks pretty-render wiring tests** (#1169, #1223). Closes the lone remaining cheap-to-action item from the v0.2.25 P3 audit bundle. Two ~30-line tests in `tui_render::tests` assert the live TUI dispatch actually invokes the `wait_task_format` pretty-printer for these tools—without them, a one-character typo in the dispatch (e.g. `"WaitTasks"` vs `"WaitTask"`) would silently fall through to raw-JSON dump. The other 6 P3 items in #1169 self-resolved during the carry period (5 by direct fix, 1 by deletion-via-refactor)—an excellent real-world signal that the koda-release skill's “bundle P3 + age out after 2 cycles” rule is working as intended.
- **Composer module extraction**: `slash_popup` + `history_nav` lifted from `tui_handlers_inference.rs` into `koda-cli/src/composer/` submodules (#1187, #1190). No behaviour change; pure file-hygiene split.
- **Stale doc-comment cleanups** (release-prep audit): updated references to `widgets/key_hints.rs` (deleted in #1183) and `widgets/slash_menu.rs` (deleted in #1190) inside `composer/key_hint.rs` and `widgets/shortcuts_overlay.rs`. One typo fix (`Plaiaction` → `Plain action`).

### Sub-agent audit (pre-release)

Per the koda-release skill's Phase 2, four sub-agents validated this release:

| Agent | Outcome |
|---|---|
| `code-reviewer` | 2 P1 (release-prep mechanics: version bump + CHANGELOG promote, both addressed in this PR), 2 P2 (`tui.md` slash-block doc patch — addressed; `#[non_exhaustive]` recommendation — deferred to follow-up to keep release surface minimal), 2 P3-bundle (typo + stale doc-comments — fixed inline). |
| `rust-programmer` | All 4 quality gates clean (`fmt`, `clippy --workspace --all-targets -- -D warnings`, `RUSTDOCFLAGS=-Dwarnings cargo doc`, lib tests 2098/2098). Integration `cargo test --workspace` deferred to CI per skill rule (already passed on all 20 source PRs). Tokio runtime-flavor audit clean. Cargo metadata publish-ready. |
| `security-auditor` | 🟢 Sandbox unchanged (`koda-sandbox/` diff is empty), zero new `unsafe`, zero new credentials, zero new FS/exec surface. One trust-policy reclassification (#1212) reviewed and confirmed correct. `cargo audit` deferred to CI's `Security Audit` job (passed on all source PRs). |
| `qa-expert` | 🟢 Coverage above target on every headline change (BgChildActivity 10 tests, overlay 27 tests, cancel cascade 5 tests including pre-fix repro, context cache 6 tests + bench, etc.). No flake debt, no wrongly-ignored tests. Manual smoke checklist included in release PR body. |

## [0.2.27] - 2026-04-29

**The composer-port release.** v0.2.27 lands the full 6-PR codex `bottom_pane::textarea` epic (#1116, #1175, #1178) — koda now owns its input composer end-to-end, the `ratatui-textarea` third-party dep is gone, and four user-visible features ride along: vim mode, the key-hint footer, atomic `@`-mention deletion, and masked rendering for API key entry. Two non-composer wins also ship: alt-screen mouse-mode cleanup is hardened against signal exits (#1177) and the CI bubblewrap install is no longer flaky on Azure-mirror outages (#1180).

### Added

- **`/vim` slash command** toggles vim-mode editing in the input composer (#1182, PR 3 of #1178). Insert ↔ Normal modes with `Esc`/`i`, full motion set (`h j k l`, `w b e`, `0 $`, `gg G`), edit primitives (`x dd yy p`, `dw db de`, `cc ci<delim> ca<delim>`), and undo/redo. Per-session toggle; documented in [docs/src/keybindings.md → Vim mode](https://github.com/lijunzh/koda/blob/main/docs/src/keybindings.md#vim-mode).
- **Key-hint footer** below the input composer (#1183, PR 4 of #1178). A single-line context-sensitive footer shows the relevant bindings (e.g. `Enter send · Alt+Enter newline · Tab complete`), updating based on whether the input is empty, has content, has an active dropdown, or is in vim mode.
- **Atomic `@`-mention named elements** in the composer (#1184, PR 5 of #1178). `@file.rs` now renders as a single cyan-styled element; `Backspace` deletes the whole `@mention` at once instead of character-by-character, matching codex's behavior.
- **Masked rendering for API key entry** (#1185, PR 6 of #1178). The `/key` flow now renders each character as `•` while you type, so a peer glancing at your terminal during a screen-share won't see the key. Backspace and paste behave normally; the real characters are tracked under the hood.

### Changed

- **Input composer migrated from `ratatui-textarea` to the in-tree codex port.** `koda-cli/src/composer::TextArea` now backs the chat composer, the modal wizards (login, settings, sessions, etc.), and every other input surface. The `ratatui-textarea` v0.9 dependency is dropped from `koda-cli/Cargo.toml` and `Cargo.lock` (#1181, PR 2 of #1178). User-facing behavior is preserved; the swap unlocks the four features above.
- **Alt-screen mouse-mode cleanup hardened against signal exits** (#1177). A `libc::atexit` hook now restores terminal mouse mode on `SIGTERM`, `SIGHUP`, and unexpected `std::process::exit()` paths, so the terminal no longer stays in mouse-capture mode after a non-graceful koda exit. Native click-and-drag text selection is also no longer broken during a healthy koda session (we now use scoped capture, not `EnableMouseCapture`). Resolves #1176.

### Fixed

- **CI bubblewrap install flakes against the Azure Ubuntu mirror are now retried** (#1180). The sandbox-test job no longer fails the entire build when `apt-get install bubblewrap` hits a transient mirror timeout.

### Internal — codex textarea port foundation (#1116, #1175, #1178)

Foundation work for swapping `ratatui-textarea` (third-party, MIT) for a
port of codex's `bottom_pane::textarea` module. PR 1 of 6 in the staged
epic; the user-visible swap and feature wins above are PRs 2-6.

- New `koda-cli/src/composer/` module tree at codex SHA
  `d55479488e125ef7a0a8584505d839a22eaf6204` (verified at parity with
  codex HEAD `35aaa5d9fc` — zero diff across all six ported files):
  - `key_hint.rs` (verbatim vendor, 285 LoC, 9 tests)
  - `paste_burst.rs` (verbatim vendor, ~430 LoC, 5 tests)
  - `text_element.rs` (reduced port, ~155 LoC, 4 tests)
  - `wrapping.rs` (reduced port, 145 LoC)
  - `keymap.rs` (koda-OWNED policy layer, ~510 LoC, 4 tests)
  - `textarea.rs` (verbatim vendor with adaptations, 3,503 LoC, 53 tests)
- Test gain: +75 tests in the composer suite (cumulative across stages),
  including +18 vim-mode coverage from the HEAD-aligned re-port.
- Quality gate: full koda-cli lib suite at **572/572 passing**, full
  workspace at **2,349/2,349 passing**, zero regressions.
- Provenance: see file headers for SHA anchor + adaptation lists.
  Future codex syncs are 3-way merges with `d55479488` as the upstream
  base. Two follow-ups remain (filed as #1186 image paste wiring, #1187
  composer module extraction).

### Internal — release-time polish (#1174)

- Knocked off 3 of 7 P3 items from the v0.2.25 audit bundle (#1169):
  no user-visible change, just opportunistic cleanups landing in the
  release-prep window per the koda-release skill's audit-dust rule.

## [0.2.26] - 2026-04-29

The RFC #1167 release. `/export` is gone; `/debug-bundle` takes its
place with a richer self-contained `.zip` artifact (conversation,
raw messages, metadata, allowlist-filtered env, and per-process
tracing logs). Panic-hook breadcrumbs make the bundled logs
self-correlating. Net workspace deletion across the three landing
PRs: **−677 LoC** — the new feature is smaller than the code it
replaces.

### Behaviour change — `/export` removed, `/debug-bundle` is the export path

- **`/export` and the `transcript.rs` module are gone.** Any user
  workflow that relied on `/export <file.md>` to dump a session
  transcript must switch to `/debug-bundle`. The new artifact is a
  `.zip` (not a `.md`) and contains the conversation rendered via the
  same `history_render` pipeline as the live TUI — byte-for-byte
  identical to what was on screen. See
  [`commands.md#debug-bundle`](./docs/src/commands.md#debug-bundle)
  for the full bundle layout, env-var redaction policy, and usage
  examples. (RFC #1167 — PR δ — #1172)
- **`KODA_TRANSCRIPT_HYPERLINKS` and `KODA_EXPORT_VERBOSE` env vars
  are gone.** Both controlled `/export` formatting; setting them is
  now a silent no-op. Remove them from your shell rc files.
  Documented under `### Removed` below.

### Added

- **`/debug-bundle` slash command** — writes a self-contained `.zip`
  to `~/.config/koda/debug-bundles/` capturing everything a debugger
  (human or LLM) needs to reason about a session: `conversation.md`
  (rendered via `history_render` — same as the live TUI),
  `messages.json` (raw DB rows), `metadata.json` (session/runtime
  context), `env.txt` (allowlist-filtered env vars, credentials
  length-redacted), and `logs/` (full per-process tracing log + panic
  log if any). Format chosen for random-access reads (LLM debugging
  poke-one-file workflows are O(file) instead of O(bundle)) and broad
  cross-platform UX. Replaces the legacy `/export` command (see
  Removed below) per RFC #1167. (#1167 — PR α)

### Changed

- **Panic hook now emits a `tracing::error!` breadcrumb** before
  writing `panic.log`. The breadcrumb — a single-line
  `thread '<name>' panicked at <location>: <message>` mirroring the
  rustc default format — lands in the per-process tracing log
  (`koda-{PID}.log`). When both files end up in a `/debug-bundle`,
  the panic is now correlatable with the surrounding tracing context
  via wall-clock timestamp, instead of sitting in `panic.log`
  isolated from the events leading up to it. (RFC #1167 — PR β)

### Removed

- **`/export` slash command and the entire `transcript.rs` module**
  (RFC #1167 — PR δ). `/debug-bundle` is now the only path for
  exporting session content. `conversation.md` inside the bundle is
  rendered via `history_render` — the same pipeline the live TUI
  uses — eliminating the divergence that motivated the v0.2.21–
  v0.2.24 audit cycle (#1162, #1164, #1165). Net deletion: ~1700 LoC
  (`transcript.rs` 1712 lines + handler + helpers + tests − small
  follow-on additions). The `/debug-bundle` success message now also
  surfaces `~/.config/koda/logs/latest` for raw-log discoverability
  (replacing the originally-planned dedicated `/logs` slash command;
  YAGNI).
- **Env vars `KODA_TRANSCRIPT_HYPERLINKS` and `KODA_EXPORT_VERBOSE`**
  removed alongside `/export`. Both controlled formatting of the
  removed transcript exporter. Setting either is now a silent no-op.
  Migration: remove from shell rc files; `/debug-bundle` has no
  equivalent flags because its output is structured (zip + JSON +
  Markdown) rather than a single concatenated stream.

## [0.2.25] - 2026-04-30

Background-tasks UX release. 5 PRs since v0.2.24 reshape `WaitTask` into
an atomic multi-task gather (#1161), polish the multi-sub-agent UX with
a status-bar pill and verbose-export escape hatch (#1160), and fix
three user-reported rendering regressions where multi-task `WaitTask`
results dumped raw JSON instead of pretty-printed per-task summaries
(#1162, #1164, #1165). The shared formatter primitives now live in a
single `wait_task_format` module so the live TUI, resumed history, and
`/export` transcript all render identically.

### Behaviour change — `WaitTask` schema is now multi-task

- **`WaitTask` request shape changed** from `{"task_id": "agent:1"}`
  to `{"task_ids": ["agent:1", "agent:2", ...]}` (#1157, PR #1161).
  The response shape changed correspondingly to a `{tasks: [...],
  summary: {...}}` envelope with per-task `status` and error
  isolation — a failure in one task no longer fails the whole call.
  This is **breaking for any system prompt, skill, or external
  agent that hardcoded the v0.2.24 single-task shape**. Single-task
  waits still work; the array just has length 1. Full schema and
  migration notes in [tools.md](./docs/src/tools.md#waittask).

### Behaviour change — sub-agent "started" message format

- **Sub-agent started messages now use `agent:N` instead of `task N`**
  (#1158, PR #1160). The model uses these IDs as-is when constructing
  `WaitTask({task_ids: ["agent:1"]})` calls, so the prefix matches
  what `WaitTask` consumes. This is observable in the transcript and
  in the LLM-facing dispatch result string.

### Added

- **Atomic multi-task `WaitTask`** (#1157, PR #1161). One tool call
  can now gather results for any number of background tasks in
  parallel, with per-task timeout, per-task error isolation, and a
  summary tally (success / failed / timeout / cancelled / forbidden
  / not_found). Replaces the previous one-task-at-a-time wait pattern
  that forced the model to round-trip per task.
- **Multi-sub-agent UX polish** (#1158, PR #1160). New status-bar pill
  shows live counts of running/waiting background agents at a glance.
  Per-iteration heartbeat lines (`Running (Nx)`) are now suppressed
  in `/export` output by default, focusing transcripts on terminal
  task outcomes. Set `KODA_EXPORT_VERBOSE=1` to restore the old
  verbose behaviour when debugging heartbeat aggregation — see
  [configuration.md](./docs/src/configuration.md#display).
- **`### WaitTask` reference subsection in `tools.md`** documenting
  the new request/response schema, per-task status values, error
  isolation semantics, and v0.2.24 → v0.2.25 migration. Promotes
  what was previously only in the model-facing tool description into
  the user-facing reference.
- **Shared `wait_task_format` module** (#1163 follow-up, PR #1165).
  Three render surfaces (live TUI, resumed history, transcript export)
  now share a single source of truth for status icons, preview-line
  extraction, and per-task summary formatting. Future status values
  added to one place light up everywhere automatically.

### Fixed

- **`WaitTask` multi-task results no longer render as raw JSON**
  (#1157 follow-up, PR #1162). The transcript exporter previously
  dumped the entire `{tasks: [...], summary: {...}}` envelope
  verbatim instead of pretty-printing per-task summaries. Now
  renders as a structured per-task list with status icons, agent
  names, and preview lines.
- **`tool_id_to_name` mapping survives Gemini's per-turn tool ID
  reuse** (#1164). Gemini reuses the same tool_call_id across
  multiple turns within a session, which collided with the
  exporter's id→name map and caused later turns to mis-render the
  tool name. The map is now built incrementally per-turn rather
  than session-wide.
- **Live TUI and resumed history render `WaitTask` identically to
  `/export`** (#1163 follow-up-2, PR #1165). Two more rendering
  surfaces had been dumping raw JSON; both now route through the
  shared `wait_task_format::try_render_wait_task_lines` primitive
  introduced in this release. The same id-collision fix from
  #1164 was applied to `history_render` so resumed sessions also
  benefit.

## [0.2.24] - 2026-04-29

Reliability + observability release. 10 commits since v0.2.23 covering:
a safety reintroduction for sub-agents (a 30-turn ceiling with a
gemini-pattern grace turn brings back bounded execution after v0.2.23
removed the hard cap), a new forensic panic-log feature for crash
diagnosis, two TUI hot-loop perf improvements (frame coalescing and
bounded event draining), a startup memory-load fix, security
hardening on the new panic-log file mode, and continued pre-1.0 API
hardening on `koda-core/bg_agent` types.

### Behaviour change — sub-agent execution is bounded again

- **Sub-agents now cap at 30 turns with a grace turn** (#1135, PR
  #1153). v0.2.23 removed `MAX_SUB_AGENT_ITERATIONS` based on the
  reasoning that modern models terminate cleanly. Real usage showed
  read-only explorer agents could spin for 100+ seconds on broad
  prompts before the model decided to stop, with no user feedback in
  the TUI. The fix matches gemini-cli's pattern: a soft 30-turn cap,
  then one **grace turn** where the model is told "you've hit the
  limit, summarize and stop" before the loop is forcibly terminated.
  Tool calls dropped during the grace turn are reported back to the
  parent agent (and surfaced in the transcript) via a
  `[max_turns reached: ...]` marker so the parent can decide whether
  to retry with a narrower scope. **If you customised sub-agent
  prompts based on v0.2.23's no-cap messaging, your agents will now
  be bounded again** — the mechanism differs (soft cap + grace turn
  vs. hard ceiling), the practical effect is similar.

### Added

- **Forensic panic log** (#1122, PR #1152). Panics now write a
  structured backtrace record to `~/.config/koda/logs/panic.log`,
  including ISO-8601 timestamp, koda version, panic message, file
  and line, and the captured backtrace (when `RUST_BACKTRACE=1` or
  `=full`). The hook is panic-in-panic safe (in-memory buffering
  before write, all I/O failures swallowed) and rotates at 5 MiB
  with 3 generations kept. Bounded disk growth even under
  deterministic crash loops. New `troubleshooting.md` mdBook page
  documents the location and usage.
- **`#[non_exhaustive]` on `BgTaskSnapshot` and `BgAgentResult`**
  (#1130, PR #1155). Future fields (e.g. token usage, parent task
  id, daemon-side metadata) can be added without it being a
  breaking change. Both structs are constructed only inside
  `koda-core`, so the attribute imposes zero friction on consumers
  — they only read fields. Pre-1.0 hardening ahead of the daemon
  epic (#1150).
- **`#[must_use]` on snapshot, result, and outcome types** —
  `BgTaskSnapshot`, `BgAgentResult`, `CancelOutcome`, `WaitOutcome`
  (#1130, PR #1155). Each carries dispatch-layer-relevant
  information; silently dropping any of them is almost always a
  bug. Free safety lint with no behavioural change.
- **`koda-sandbox` crates.io category `concurrency`** (#1130, PR
  #1155). The sandbox supervises concurrent tool execution and
  sub-agent spawn, so the category is a genuine fit. Improves
  discoverability without category-stuffing.

### Fixed

- **`CLAUDE.md` no longer loads twice on startup** (#1136, PR #1151).
  Previously the agent loaded `CLAUDE.md` once during `KodaAgent::new()`
  and then again when `rebuild_system_prompt()` ran on first turn,
  producing duplicate `"Loaded N tokens from CLAUDE.md"` log lines
  and unnecessary disk I/O. Loaded memory is now cached on the
  agent and reused.

### Performance

- **Coalesced TUI draws via frame scheduler** (#1138, PR #1143).
  The inference loop previously called `terminal.draw()` on every
  ui-event arm, leading to redundant renders when events arrived
  in bursts. A new `FrameRequester` schedules a single draw per
  frame interval (60Hz default), measurably reducing CPU on
  streaming turns without sacrificing responsiveness. Eight
  `start_paused = true` tokio tests verify the scheduling logic
  deterministically.
- **Bounded drain + round-robin select fairness in TUI loop**
  (#1137 / #1139, PR #1142). Previously the inference loop drained
  every available event with an unbounded `while let Ok(_) =
  try_recv()`, which under sustained sub-agent fan-out could
  starve crossterm input (mouse-escape mis-framing per #540). The
  drain is now capped at 64 per turn and the `tokio::select!` arm
  priority rotates each iteration so no producer can monopolise
  the loop.

### Security

- **`~/.config/koda/logs/` and `panic.log` written with 0o600 / 0o700
  on Unix** (release-time defense-in-depth from the security
  audit). Backtraces can transitively contain formatted secrets
  (e.g. `panic!("auth: {key}")`); the new panic-log feature is now
  unreadable to other local users on a shared host. Windows
  inherits ACLs from the parent directory (the desired behaviour).
  Best-effort — silently no-op on systems that don't support the
  permission bits.

### Tests / CI

- **`PersistingSink` integration coverage** (#1129, PR #1154). 10
  multi-thread integration tests covering the full sink-routing
  contract: tool-call persistence, sub-agent trace folding, error
  propagation, drop semantics. Closes the test gap from #1112's
  rapid v0.2.23 landing.
- **Regression test for `read_timeout` auto-retry on silent SSE**
  (#1134). Confirms the v0.2.23 timeout fix (#1119) actually
  retries through `try_with_rate_limit` rather than hard-failing
  the turn, using a fake server that goes silent and resumes.
- **Regression test for panic-hook TTY restore + chaining** (#1133).
  Verifies the v0.2.23 panic hook (#1124) restores terminal state
  AND chains to the previously-installed hook exactly once,
  preventing both TTY corruption and double-handling. Refactors
  `install_panic_hook` to take the restore callback as a parameter
  for testability.
- **Coverage workflow treats checkout / runner failures as infra
  flakes** (#1132). The post-merge coverage job now distinguishes
  "infrastructure broke" from "coverage actually regressed" — no
  more bogus issue-filing on transient runner failures.

### Notes

- **`#[non_exhaustive]` deliberately NOT applied to `AgentStatus`,
  `CancelOutcome`, `WaitOutcome`** (#1130, PR #1155). The audit's
  blanket recommendation didn't account for `koda-cli` exhaustively
  matching these enums in three rendering / dispatch paths — a
  missed-variant compile error there is a *feature*, not a wart.
  Attaching `#[non_exhaustive]` would have downgraded those checks
  to wildcards and silently hidden bugs the next time we add a
  status variant.
- **`KodaAgent` deliberately NOT marked `#[non_exhaustive]`** even
  though a new `pub semantic_memory` field was added in #1136. The
  release-time code review flagged this as a P2 consistency item;
  the cost-benefit analysis was: 3 integration test fixtures
  construct `KodaAgent` via struct literal (lightweight, no
  `KodaConfig` plumbing), and `koda-core` is documented as
  internal-pre-1.0 in `koda-core/README.md`, so the protection
  has no real consumers today. Revisit when `koda-core` stabilises
  for external use.

## [0.2.23] - 2026-04-29

Feature + reliability release. 12 commits since v0.2.22 covering: a
behavioural shift for sub-agents (the 20-iteration safety cap is gone
— see DESIGN.md P3), two TUI quality-of-life features (persistent cwd
in the status bar, terminal-restoring panic hook), richer transcript
exports (sub-agent traces now persist and fold under their parent
`InvokeAgent` call), an inference reliability fix (longer SSE
read-timeout plus auto-retry on transient network errors), and a
dependency cleanup that removes 16 transitive crates and bumps MSRV
to Rust 1.88.

### Breaking (koda-core internals)

> `koda-core` is an internal crate published to support the `koda-cli`
> binary; its public API is not stable pre-1.0. See
> `koda-core/README.md` for the stability statement. The CLI itself
> (`koda` binary, `/commands`, TUI keybindings, config schema) has no
> breaking changes in this release.

- **Removed `pub const MAX_SUB_AGENT_ITERATIONS`** from `koda-core`
  (#1127, closes #1110). The 20-iteration ceiling on sub-agent loops
  is gone. Termination is now driven by the model (clean stop, no
  tool calls), `LoopDetector` (consecutive identical calls → feedback
  → hard stop), parent cancellation, or context exhaustion — the same
  set of mechanisms Codex and Zed rely on. Per `DESIGN.md` P3 ("Build
  for the world six months from now"), the cap was redundant
  scaffolding for a model weakness P3 explicitly says not to
  compensate for.
- **`AgentStatus::Running.iter` widened `u8` → `u32`** in
  `koda-core/src/bg_agent.rs` (#1127). With the cap gone, a
  meandering weak model could plausibly exceed `u8::MAX` before
  context exhaustion; silent wraparound would mislead the TUI status
  bar.
- **`WAIT_TASK_DEFAULT_TIMEOUT_SECS` bumped 30 → 60** seconds (#1127,
  closes #1106). Sub-agent inference rounds take real time; the
  default should mean "I'm willing to wait this long," not "check
  back quickly."
- **MSRV bumped Rust 1.87 → 1.88** (#1118). Required by the dependency
  cleanup; also unblocks `time 0.3.47` (RUSTSEC-2026-0009).

### Added

- **Persistent cwd in the TUI status bar** (#1117, #1105). The status
  bar now shows the current working directory, with `$HOME`
  substituted as `~` and the path right-truncated at segment
  boundaries to fit the available space. Live-updates on terminal
  resize. New `format_cwd_compact` helper covered by 9 unit tests.
- **Sub-agent traces persist and fold in transcript exports** (#1112,
  partial #1108). Engine `SessionEvent`s and sub-agent traces are
  now persisted to the session DB via the new `PersistingSink`. On
  `/export`, sub-agent invocations render nested under the parent
  `InvokeAgent` tool result, so multi-turn delegation is auditable
  in the exported markdown. Re-exporting an old session also
  reflects the hierarchy.

### Changed

- **Sub-agents now trust the model** (#1127, closes #1110, #1106).
  See the Breaking section for the mechanics. The TUI iteration
  display drops the `/20` denominator. The `LoopDetector` hard-stop
  message gains a "consider switching to a stronger model" hint —
  the one place we have a smoking-gun signal that the model (not
  the task) is the problem. The `InvokeAgent` tool description gains
  one line steering read-only work to `explore` (faster, cheaper,
  no isolated workspace) and writes to `task`.
- **`WaitTask` description nudge** (#1127, closes #1106): prefer
  120–300 s for sub-agent waits, call sparingly.

### Fixed

- **Inference: longer SSE read-timeout + auto-retry on transient
  network errors** (#1121). Bumped `reqwest::read_timeout` 180s →
  300s to accommodate slow reasoning models (Gemini 3.x Pro,
  MiniMax, etc.) that can silently buffer on a single SSE chunk for
  minutes. Wrapped the inference call in `try_with_rate_limit` so
  transient timeouts and connection resets retry up to 5 times with
  exponential backoff (`is_network_transient_error` predicate covers
  "operation timed out", "connection reset", "broken pipe", and
  related substrings). Mutually exclusive with the existing
  rate-limit / context-overflow / image-rejection retry paths.
- **TUI restores terminal on panic** (#1120). Installed a custom
  panic hook (modeled on `codex-rs/tui/src/tui.rs`) that disables
  raw mode, releases mouse capture, and exits the alternate screen
  before chaining to the original hook. Crashes no longer leave the
  shell in an unusable state requiring `reset`.
- **Transcript export surfaces tool name + args + call\_id** (#1111,
  partial #1108). Pre-fix, `/export` markdown showed only the
  tool's text result without identifying which tool was called or
  with what arguments. Now each tool block is properly headered.

### Internal

- **Refactor `tool_header`**: unify `detail_spans` and `detail_text`
  via a single `ToolCallSummary` source of truth (#1107). Eliminates
  the per-tool drift that contributed to #1099. Pure refactor, no
  behaviour change.
- **Test infrastructure hardening** (#1109, #1114). 37 files updated
  to: (a) replace `unsafe { std::env::set_var }` in tests with
  dependency-injection helpers (`stage2_binary_from`,
  `set_worker_binary_for_tests`); (b) add `#[tokio::test(flavor =
  "multi_thread")]` to every test that exercises code containing
  `tokio::spawn` (the default `current_thread` flavor can deadlock
  spawned tasks under macOS CI load); (c) eliminate the last
  polling-sleep in the sandbox tests. Net `unsafe` count in repo
  went down. New CI lint `scripts/check_tokio_test_flavor.py`
  prevents regressions.
- **Bundle v0.2.22 P3 release-polish items** (#1115, closes #1104).
  Worktree/submodule-aware `.git/config` skip in bwrap (covers a
  ClonefileProvider edge case); plus four small doc/test polish
  items.
- **chore(deps)**: remove 5 dead direct deps (−16 transitive crates)
  + bump MSRV to 1.88 (#1118). Smaller supply-chain surface.
- **ci(deps)**: bump `taiki-e/install-action` 2.75.19 → 2.75.25
  (#1126).

## [0.2.22] - 2026-04-28

Patch release. 8 bug-fix commits since v0.2.21, no new features, no public
API changes. Highlights: a sub-agent loop-spin bug that was burning ~20×
the LLM calls per multi-turn invocation, the matching TUI rendering bug
that hid it, a Linux sandbox TOCTOU window on `.git/config`, and four
smaller correctness fixes (Gemini schema, prompt-builder discovery,
`List` tool header, ClonefileProvider `$TMPDIR` fallback).

### Security

- **SEC-002 — sandbox now pre-creates `.git/config` to close TOCTOU
  window** (#1092). On Linux (bwrap backend), `apply_git_config_deny`
  previously skipped the `--ro-bind` of `.git/config` if the file did
  not exist at sandbox-build time. A child process inside the sandbox
  could then race to create the file before the deny took effect, and
  inject `core.fsmonitor = <cmd>` for later host-side execution. Fix
  unconditionally pre-creates `.git/hooks/` and `.git/config` (empty
  file, mtime-preserving on existing repos) before binding them
  read-only, so the deny applies regardless of repo state. Brings the
  bwrap backend to parity with Seatbelt's SBPL deny rules (which
  already covered non-existent paths). Regression test
  `git_config_deny_pre_creates_for_non_git_dir_to_close_toctou` pins
  the contract.

### Fixed

- **Sub-agents no longer spin on the same tool call until the
  iteration cap** (#1102, closes #1101). `sub_agent_dispatch` inserted
  assistant tool-call rows into the session DB but never called
  `mark_message_complete`. `load_context` filters
  `(role = 'assistant' AND completed_at IS NULL)` rows, so every loop
  iteration the sub-agent's effective context collapsed back to
  `[system, user]`; `prune_mismatched_tool_calls` then dropped the
  orphaned tool-result rows. The model re-issued the same tool call
  every iteration until hitting `MAX_SUB_AGENT_ITERATIONS = 20`,
  burning ~20× the intended LLM calls. Affected every multi-turn
  sub-agent (`explore`, `plan`, `verify`, `task`, all user-defined
  sub-agents, all background `Task` agents). One-line fix mirrors
  the parent inference loop's pattern at
  `inference.rs::mark_message_complete`. Regression test
  `sub_agent_marks_assistant_messages_complete_so_loop_progresses`.
- **TUI tool headers now show the actual file path, not always `.`**
  (#1099). The renderer in `koda-cli/src/tool_header.rs` looked up
  the wrong key in the tool-call args JSON, so `Read`, `List`,
  `Grep`, `Glob`, and friends always displayed `● List .` regardless
  of what path the model passed. Made debugging agent behavior
  effectively impossible — the loop-spin bug fixed in #1102 was
  invisible behind this for 8 days because every iteration's `List
  /Users/lijun/repo` rendered identically as `● List .`. Fix maps
  each tool's actual dispatch key to the header. Regression test
  `path_bearing_tools_render_actual_dispatch_key`.
- **Built-in sub-agents now appear in the system prompt** (#1098).
  The prompt builder called the wrong discovery function, listing
  user-installed agents but omitting `explore`, `plan`, `verify`,
  `task`. Result: the model could see them in `/agents` output but
  refused to call them with "No sub-agents are configured." Fix
  routes both surfaces through `discover_all_agents`, which also
  filters reserved names (`koda`, `default`) to prevent
  self-delegation. Regression tests
  `built_in_agents_appear_in_prompt_with_no_installed_agents` and
  `task_is_general_purpose_subagent_and_main_agent_is_hidden`.
- **Gemini provider sends tool parameters under
  `parametersJsonSchema`, not `parameters`** (#1097). Gemini's
  function-declaration schema rejects `additionalProperties` under
  the `parameters` key but accepts the full JSON Schema vocabulary
  under `parametersJsonSchema`. Pre-fix: every Gemini tool call
  returned HTTP 400 `Unknown name "additionalProperties"`. Affected
  all Gemini-family models. Regression test
  `function_declaration_serializes_under_parameters_json_schema`.
- **`List` tool output starts with a `Listing: <path>` header**
  (#1096, closes #1094). Empty-directory listings previously
  produced a bare `(empty directory)` string with no path context,
  and capped listings dropped the header too. Now every `List`
  result begins with the resolved directory path, matching
  `Read`/`Grep` behavior. Four regression tests in `file_tools.rs`.
- **`ClonefileProvider` falls back to `$TMPDIR` when
  `project_root == $HOME`** (#1095, closes #1093). `clonefile(2)`
  returns `EPERM` when the destination is a descendant of the
  source. The default `clones_root` lives at
  `$HOME/.koda/clones/<hash>/`, so running `koda` from `$HOME`
  meant every `provision()` failed and sub-agents with write tools
  could not be dispatched at all. Fix detects the recursion via a
  new `choose_clones_root` helper and falls back to
  `$TMPDIR/koda-clones/<hash>/`; if both candidates would land
  inside `project_root`, returns `Err` so `pick_write_provider`
  falls back to `GitWorktreeProvider`. Five boundary-class
  regression tests.
- **`bg_agent_iter_counter_advances_via_status_channel` test pinned
  to `multi_thread` runtime** (#1091, closes #1090). The default
  `current_thread` tokio flavor only progresses spawned tasks when
  the test task explicitly yields; on macOS CI runners under load
  this caused the test to time out at 5s polling deadline despite
  the dispatch path being fully synchronous. Production runs on
  `multi_thread`; tests now match. Diagnostic panic was added that
  dumps the events vector + final snapshot on failure, so any
  future regression in this area is actionable instantly.

## [0.2.21] - 2026-04-27

### Security

- **SEC-001 — git-deny rules now actually win SBPL resolution** (#1086).
  PR #1073 emitted `deny file-write*` rules for `.git/config` and
  `.git/hooks/` BEFORE the policy overlay's `allow file-write* (subpath ROOT)`.
  Because SBPL is last-match-wins, the cascade `allow → deny → allow`
  silently re-permitted writes — the protection in #1073 was dead code.
  Fix seeds `policy.fs.deny_write_within_allow` so the denies are emitted
  AFTER `allow_write` in both backends (Seatbelt and bwrap), making the
  protection enforce as documented. Caught during release audit.
- **Sandbox: closed git-config + git-hooks escape vectors** (#1073, #1086).
  When `allow_git_config = false` (the default), writes to `.git/config`
  (blocks `git config core.fsmonitor <cmd>`) and `.git/hooks/*` (blocks
  direct hook placement) are now denied across both Seatbelt (macOS) and
  bwrap (Linux) backends.

### Added

- **`EngineEvent::TodoUpdate { items, diff }`** (#1080). The engine now
  emits a structured event whenever the session task list changes,
  carrying both the full list and the per-task diff. Replaces the
  previous system-prompt injection mechanism.
- **`EngineEvent::BgTaskUpdate { task_id, spawner, status }`** (#1078).
  Background sub-agent status changes now flow through the engine's
  event stream instead of a separate side channel.
- **`koda_core::provider_catalog` module** (#1084). Static lookup tables
  for `ProviderType` and `ProviderMeta` extracted from `config.rs` for
  better cohesion.
- **`koda_core::bg_agent::BgStatusEmitter`** (#1078). Public type for
  routing background task status updates through the engine.
- **`koda_core::tools::todo::{TodoChange, TodoDiff, TodoWriteOutcome}`**
  (#1080). New public types describing structured `TodoWrite` results.
- **Max-1-in-progress validation for TodoWrite** (#1080). The tool now
  rejects task lists with more than one in-progress item, enforcing the
  single-focus invariant promised by the user-facing prompt.

### Changed

- **`koda_core::tools::todo::todo_write` return type**
  `Result<String>` → `Result<TodoWriteOutcome>` (#1080). Direct callers
  of `koda-core` need to update their call sites; the CLI consumes via
  the engine and is unaffected.
- **Progress tracking is now event-driven, not prompt-injected** (#1080,
  #1081). `TodoWrite` is the canonical mechanism for task tracking;
  the engine emits `TodoUpdate` events to interested observers (the
  TUI, sub-agent callers). System prompt no longer carries progress
  text, which made compaction smarter and removed a class of subtle
  drift bugs.

### Removed

- **`koda_core::progress` module** (#1081). The pre-#1077 architecture
  injected progress summaries directly into the system prompt; this is
  now handled via `EngineEvent::TodoUpdate`. The module and its
  `track_progress` / `get_progress_summary` helpers are gone.
- **`koda_core::tools::todo::get_todo_section`** (#1080). No longer
  needed; consumers receive todo state via `TodoUpdate` events.
- **`insta` dev-dependency** (#1071). Snapshot tests rewritten as
  `assert_eq!` to drop a heavyweight dev dep; the `similar` crate is
  unified at 3.x across the workspace.

### Fixed

- **`List` tool results were never being microcompacted** (#1085). The
  `COMPACTABLE_TOOLS` constant carried both PascalCase and snake_case
  spellings, but lookups happened against canonicalized names that had
  already been normalized to PascalCase — so the snake_case entries
  were dead and the PascalCase `List` happened to be missing. Fix
  canonicalizes at lookup and drops the dual-case anti-pattern; adds a
  drift guard test to prevent recurrence.
- **P3 polish bundle for #1045** (#1070). Doc-test comments,
  `bg_agent.subscribe` correctness, QA-001 iteration test.

### CI / Internal

- **Fail-fast release test matrix** (#1069). macOS test job is
  cancelled when the Ubuntu job fails first, cutting feedback latency
  on broken PRs from ~25min to ~10min.
- **Architecture audit reflected in DESIGN.md** (#1075, #1079).
  Documentation rewritten to match the actual implementation post-#1077;
  no behavioral change.

## [0.2.20] - 2026-04-26

### Added

- **Live iteration counter in background agent status** (#1058) — `/agents`
  and the status-bar pill now show `▶ Running (iter N/20)` as background
  sub-agents progress through inference iterations, instead of always showing
  a flat `▶ Running`.

### Removed

- **`TodoRead` tool removed** — the session task list is write-only;
  `TodoWrite` is the only task-tracking tool. The model sees `TodoWrite`
  only, removing a redundant read path.
- **`session_id` parameter removed from `InvokeAgent` tool schema** (#1056).
  Sub-agent sessions are always freshly created; the parameter was never
  honoured and caused model hallucination.
- **`koda_core::approval` re-export shim deleted**. Downstream code should
  use `koda_core::trust` directly (`TrustMode`, `check_tool`, etc.).
- **Speculative dead wire-protocol types removed** from `EngineCommand`:
  variants `UserPrompt`, `SlashCommand(SlashCommand)`, `Quit`; structs
  `ImageAttachment` and `SlashCommand`. All were `#[allow(dead_code)]` with
  zero consumers.

### Fixed

- **B19 child-trust regression** (#1022) — child trust mode is now clamped
  to the parent's *runtime* mode (e.g. after `/safe`), not the startup
  config mode. Switching to `/safe` mid-session now correctly prevents
  sub-agents from inheriting the original, wider trust level.
- **RUSTSEC-2026-0097 release-gate expanded** (#1050) — the CI grep that
  verifies the `rand` unsoundness suppression now covers all workspace
  `src/` directories (`koda-cli`, `koda-core`, `koda-sandbox`), not just
  `koda-cli/src/`.

### Internal

- `once_cell` dep removed from `koda-cli`; replaced with
  `std::sync::LazyLock` (stable since Rust 1.80).
- `repl.rs` deleted (−965 LOC); slash-command parse+dispatch folded into a
  single `match` in `tui_commands.rs`. Adding a slash command now requires
  one edit instead of four.
- `koda-sandbox/Cargo.toml` now inherits `homepage`, `authors`, `readme`,
  and gains `keywords`/`categories` for crates.io discoverability.
- RUSTSEC-2026-0097 suppression carries a tracking issue (#1050), an
  exposure rationale, and an exit condition (tungstenite ≥ 0.30).

## [0.2.19] - 2026-04-26

Infrastructure release. Fixes the crates.io publish pipeline that was
broke in v0.2.18, and supersedes that release. No API or behaviour changes.

> **v0.2.18 note:** the GitHub Release binaries for v0.2.18 are correct and
> usable, but `koda-core` and `koda-cli` were never published to crates.io
> (`cargo publish` died because `koda-sandbox` lacked a `version` field).
> v0.2.18 has been marked pre-release on GitHub to avoid confusion.
> `cargo install koda-cli` will install v0.2.19.

### Fixed

- **`koda-sandbox` missing `version` in `koda-core` dependency caused
  `cargo publish` to fail** (#1052). `koda-core/Cargo.toml` declared
  `koda-sandbox = { path = "../koda-sandbox" }` with no `version`. When
  cargo publishes and strips `path`, it has no constraint to substitute and
  rejects the manifest outright. Fixed by:
  1. Removing `publish = false` from `koda-sandbox/Cargo.toml` — the crate
     is now a first-class published workspace member.
  2. Bumping `koda-sandbox` from `0.1.0` to match the workspace version
     (`0.2.19`) so all three crates are versioned in lockstep.
  3. Adding `version = "0.2.19"` to the `koda-sandbox` dep in
     `koda-core/Cargo.toml`.
  4. Expanding `release.yml` `verify-version` to check `koda-sandbox`
     alongside `koda-core` and `koda-cli`.
  5. Prepending a `Publish koda-sandbox` + sparse-index-wait step to the
     `publish` job so the dep is in the registry before `koda-core` resolves
     it.

## [0.2.18] - 2026-04-26

Large architectural release with two main threads: a full kernel-sandbox
overhual (`koda-sandbox` crate) and a comprehensive background sub-agent
hardening pass (22 lifecycle fixes). Also ships the background-task management
UI and three new LLM meta-tools.

### Added

- **Background task system — Layer 0: `AgentStatus` + watch channel + per-task
  cancel** (#996, #1041). `BgAgentRegistry` now tracks running sub-agents
  with a typed `AgentStatus` enum (`Running`, `Completed`, `Failed`,
  `Cancelled`, `TimedOut`) broadcast over a `tokio::sync::watch` channel.
  Every agent gets an individual cancel token so the parent can terminate a
  specific task without killing siblings.

- **Background task system — Layer 1: `/agents` and `/cancel` slash
  commands** (#996, #1042). `/agents` renders a live table of all background
  tasks in the TUI. `/cancel <id>` accepts both `agent:N` and `process:N`
  prefixed forms and routes to the right registry. Bare numeric
  (`/cancel 7`) aliases to `agent:7` for back-compat. Process status spans
  use their own palette (`Running` / `Killed` / `Exited (code)`).

- **Background task system — Layer 2: `ListBackgroundTasks`, `CancelTask`,
  `WaitTask` LLM tools** (#996, #1043). Three new model-callable meta-tools
  giving the LLM first-class access to the task registry. `WaitTask` polls
  up to 300 s and returns a typed payload (`status`, `output_preview`).
  Caller-scoping is enforced: a sub-agent cannot see or cancel a sibling's
  tasks; the TUI (human operator) is unscoped and sees everything.

- **Background task system — unified TUI surface + meta-tool wiring** (#996,
  #1044, Phase F+G). `/agents` and `/cancel` now cover *all* background
  tasks (sub-agents + shell processes) in one unified table. `ListBackgroundTasks`,
  `CancelTask`, and `WaitTask` are promoted to `skill_scope::META_TOOLS`
  alongside `ActivateSkill` / `InvokeAgent` / `AskUser` — skill-scoped
  agents can manage their own background work without re-listing the tools.

- **`koda-sandbox` crate extracted** (#934, #984). All kernel-sandbox
  machinery lifted from `koda-core` into an independent `koda-sandbox` crate
  with `publish = false`. Clean dependency boundary: `koda-core` depends on
  `koda-sandbox`; the sandbox has no back-reference.

- **Sandbox violation reporting + two-layer policy overlay** (#934, #985).
  Structured `PolicyViolation` events with source attribution; child sandbox
  policy always narrows from the parent (union of deny sets, intersection of
  allow sets, minimum of limits). Compose invariant: `SandboxPolicy::compose`
  can only restrict, never widen.

- **Sandboxed filesystem over IPC** (#934, #986–#989). Full
  `FileSystem` trait with `LocalFileSystem` and `SandboxedFileSystem`
  implementations. All five file tools (`Read`, `Write`, `Edit`, `Glob`,
  `Grep`) thread the `FileSystem` trait so they can run against a
  sandbox-isolated worker via Unix socket transport.

- **`GitWorktreeProvider`; `WorkspaceProvider` trait** (#934, #990). Sub-agent
  dispatch uses `WorkspaceProvider` — swap between local CWD, git worktree
  clone, or APFS reflink slot without touching dispatch logic.

- **Path defense primitives + worker policy gate** (#934, #991). Symlink
  escape detection (full chain walk + cycle detection), FIFO/socket/device
  blocking, `is_dangerous_system_path` guard, and `mandatory_deny_search_depth`
  floor to prevent shallow symlink walks in Plan mode.

- **Built-in HTTP CONNECT egress proxy with hostname allowlisting** (#934,
  #994). Outbound HTTP/HTTPS traffic from sandboxed processes routes through
  koda's built-in proxy. The allowlist uses RFC 6125 single-label wildcard
  matching — `*.example.com` matches `api.example.com` but not
  `a.b.example.com`; bare `*`, `*foo.com`, and `foo.*` are rejected at
  construction time.

- **Kernel-enforced egress proxy** on macOS (seatbelt, #934, #997) and
  Linux (bwrap network namespace, #934, #998).

- **SOCKS5 proxy + corporate `HTTPS_PROXY` chaining** (#934, #999).
  Multi-hop proxy stack: built-in CONNECT proxy → upstream SOCKS5 →
  corp `HTTPS_PROXY`.

- **`SandboxPool` + `SandboxSlot` lifecycle** (#934, #1004). Pool of
  pre-warmed sandbox slots; `reserve()` → `attach()` two-phase handoff
  ensures oneshot sender is in hand before the task handle is captured.
  Distinct policy buckets never share a slot.

- **APFS reflinks via `ClonefileProvider`** (#934, #1005). On macOS,
  workspace slots are provisioned with `clonefile(2)` (copy-on-write)
  for near-zero-cost snapshot isolation. Falls back gracefully on
  non-APFS volumes (`ENOTSUP`).

- **Phase 5 telemetry hooks** (#934, #1010). Structured event callbacks
  for sandbox slot acquire/release, policy decisions, and egress decisions.

- **`SandboxPolicy` constructor + sub-agent threading** (#934, #1014–#1017).
  `SandboxPolicy::compose` wired through all sub-agent dispatch paths.
  Trust-derived `mandatoryDenySearchDepth` (min 8, non-bypassable) wired
  into `paths_for_write_check`. IPC-only deserialize guard prevents
  `SandboxedFileSystem` from being constructed outside the IPC transport.

- **CPU / RSS / FD resource limits via `setrlimit(2)`** (#934, #1020).
  Per-trust wall-time defaults. Unsatisfiable limits fail closed at spawn
  time rather than silently continuing uncapped.

- **`max_output_bytes` limit threaded through to sandbox** (#934, #1021).
  Sandbox worker respects the same output-size cap as the in-process tools.

### Changed

- **`koda-sandbox` README with threat model + escape hatches** (#934, #1018).
  Documents accepted risks (credential exfiltration via network), the
  `koda/db` full-block rationale (API keys in plaintext), and the
  sandbox escape hatches.

- **`InvokeAgent` tool description expanded** (#1003, #1037). Model now
  receives explicit guidance on the four execution modes and when to use
  each, reducing misrouted dispatch in practice.

- **Architectural patterns documented** (#1022, #1036). `DESIGN.md` covers
  the three patterns hardened during the multi-agent execution review:
  B18 (iteration cap), B20 (fork-history atomicity), B21
  (workspace-provision short-circuit).

- **Pre-push hook trimmed to fmt-only** (#1040). CI now handles the full
  lint/test gate; the local hook only runs `cargo fmt --check` for fast
  feedback. `scripts/preflight.sh` removed entirely.

- **CI split into parallel lint jobs** (#979). `fmt`, `clippy`, `check`,
  `test`, `doc`, and `audit` run in parallel where possible, cutting
  median wall-clock time on PRs.

- **docs: unified bg-task surface** (#1044). `docs/src/commands.md` and
  `DESIGN.md` document the prefixed-id model, spawner-scoping rule,
  and the four-layer rollout.

### Security

- **`derive_child_trust` uses runtime mode, not stale `parent_config.trust`**
  (#1022, B19, #1033). Sub-agents previously clamped their trust against the
  startup `parent_config.trust` value. If the user changed the trust mode
  mid-session (e.g., `/safe` after starting in Auto), child agents could
  still inherit the original elevated trust. Now `derive_child_trust` reads
  the runtime mode from the session state. **This was a real privilege
  escalation vector — the fix is a release-day security priority.**

- **Auto mode now hard-fails when kernel sandbox is unavailable** (#934,
  #1013). Previously, if `bwrap` (Linux) or seatbelt (macOS) was absent,
  Auto mode silently fell back to running without a sandbox. Now it
  `bail!`s with a clear error. The sandbox is the primary security boundary
  in Auto; silent fallback was a sharp footgun.

### Fixed

- **Sub-agent lifecycle hardening — 22 fixes (B1–B22)** (#1022, #1025–#1035).
  Comprehensive pass over the background agent dispatch path. Key fixes:
  - **B1–B4**: parent trust enforced, cancel token cascade, sandbox policy
    inheritance, lifecycle state machine.
  - **B5**: switched `tokio::task::spawn_blocking` → `tokio::spawn` for
    async sub-agents (eliminates thread-pool starvation).
  - **B6–B8, B14**: unified dispatch path — cache, nested invoke, `AskUser`,
    and validation all go through a single path, eliminating drift bugs.
  - **B9, B15**: background agent visibility in TUI; headless reject signaling.
  - **B10–B12**: background agent lifecycle state transitions.
  - **B13**: `can_parallelize` uses tracker-aware classification.
  - **B16–B17, B22**: P2 robustness cluster (iteration guard, workspace
    cleanup, parallel dispatch edge cases).
  - **B18**: sub-agent iteration cap returns a structural `Failure` marker
    and caches it, preventing runaway retries.
  - **B20**: `fork` copies parent history in a single database transaction
    (was row-by-row, causing torn forks under concurrent writes).
  - **B21**: workspace-provision failure now short-circuits write sub-agents
    instead of silently dropping them from the parent tool-call tree.

- **Long `AskUser` questions now word-wrap instead of truncating** (#1024,
  #1039). Questions wider than the terminal width wrap correctly at word
  boundaries.

- **OSC 8 hyperlink width contained to one cell** (#995, #1038). Ratatui's
  diff renderer was inflating the cell width of path rows that contained
  OSC 8 escape sequences, causing trailing characters to be dropped on
  re-render.

- **`Write` / `Edit` / `Delete` tools now allowed in tempdirs** (#947,
  #978). The sandbox path-check was blocking writes to `/tmp` and `$TMPDIR`
  even when the operation was safe. Tempdirs are now explicitly allowed.

- **Context meter no longer exceeds 100%** (#946, #977). Multi-tool-call
  turns were accumulating token counts across all tool calls in the turn,
  causing the context percentage display to overflow.

- **`--mode` flag honoured at session creation** (#982, #983). The
  `--mode` CLI flag was being ignored during session initialisation;
  the session always started in the default trust mode regardless.

- **Sandbox tests shielded from ambient `HTTPS_PROXY`** (#1008, #1009).
  Tests that exercised the built-in proxy were failing in corporate
  environments where `HTTPS_PROXY` is set globally. Tests now
  explicitly unset proxy env vars before running.

- **`ClonefileProvider` wired on macOS + divergence documented** (#1007).
  macOS builds now select `ClonefileProvider` over `GitWorktreeProvider`
  for slot workspaces; the different cost profiles (COW vs. worktree
  clone) are documented in `koda-sandbox/src/workspace.rs`.

## [0.2.17] - 2026-04-19

Safety release. Closes five active classifier-bypass bugs in the bash command
classifier — four of which were silently auto-approving destructive operations
in Safe mode. The classifier now correctly handles bare pipeline tails,
command-runners (xargs, env), find with destructive flags, subshells, and
process substitution. No `koda-core` API changes — patch-compatible with
v0.2.16.

### Fixed

- **Bare pipeline tails now auto-approve** (#944). Pipelines ending in a bare
  read-only command like `grep foo *.rs | sort`, `find . | wc`,
  `cat file | uniq` were misclassified as `LocalMutation` and prompted for
  approval in Safe mode, even though every segment is in the read-only
  allowlist. Root cause was the `READ_ONLY_PREFIXES` matcher's two-branch
  design where the trailing-space convention (e.g. `"sort "`) only matched
  commands followed by space — not bare commands at end of pipeline.
  Unified into single-branch matcher; substring false-positives still rejected.

- **`xargs <cmd>` no longer auto-approves in Safe mode** (#968). `xargs` was
  in `READ_ONLY_PREFIXES`, so `ls | xargs rm`, `find . | xargs mv`, etc.
  would auto-approve without confirmation. `xargs` is fundamentally a
  command-runner, not a read-only filter. Removed from allowlist; any
  `xargs <cmd>` now requires approval. **Real Safe-mode escape hatch
  closed** — `rm -rf` in raw text was caught by Phase 1 patterns, but
  `xargs rm` evaded it because `rm` wasn't directly in the command line.

- **`env <cmd>` no longer auto-approves in Safe mode** (#970). Same bug
  class as xargs. `env cargo build`, `env FOO=bar rm file`,
  `env make install` all auto-approved because `env` was in
  `READ_ONLY_PREFIXES`. Removed from allowlist; bare `env` now requires
  approval too — use `printenv` for read-only environment variable inspection.

- **`find` with destructive flags now classified as Destructive** (#970
  sweep). Eight flags that turn `find` into a deletion / command-runner /
  file-writer: `-delete`, `-exec`, `-execdir`, `-ok`, `-okdir`, `-fprint`,
  `-fprintf`, `-fls`. Previously all auto-approved because `find ` is in
  the read-only prefix list. Now each flag forces approval via dedicated
  `DangerCheck::CmdFlag` entries. Plain `find . -name '*.rs'` etc. stays
  read-only.

- **Subshells `(rm -rf /)` and command groups `{ rm; }` correctly classified
  as Destructive** (#972). Previously `(rm -rf /tmp/x)` was `LocalMutation`
  (still asks in Safe mode but auto-approves in Auto/YOLO mode) because the
  leading `(` made the `rm` not appear at token index 0. Now strips
  outermost subshell/group brackets in `classify_segment` so the inner
  command is classified correctly. Read-only inner commands like `(ls -la)`
  stay read-only.

- **Process substitution `<(cmd)` / `>(cmd)` no longer hides destructive
  commands** (#973 — **CRITICAL**). `cat <(rm /tmp/x)` was classified as
  `ReadOnly` because shlex tokenises `<(rm` as a single literal token,
  hiding the `rm` from danger checks. **Real Safe-mode escape hatch
  closed** — a model could compose `cat <(rm tracked-file)` and koda would
  auto-approve it. Added `<(` and `>(` to `RAW_DANGER_PATTERNS` to match
  existing handling of `$(`. Quoted occurrences (`echo '<(...)'`) stay safe
  via existing quote-stripping.

### Internal

- **Bash classifier hardened against entire bug class.** Added 20+ new test
  cases across the `bash_safety` module covering: bare pipeline tails,
  command-runner allowlist gaps, find destructive flags, subshells, brace
  groups, process substitution, mixed pipelines containing destructive
  segments, and quoting boundaries. Total `bash_safety` tests went from
  11 → 35.

- **Five-PR investigation cascade.** Started from one user bug report
  (#944), each PR's tests surfaced the next bug:
  - #944 → PR #969 → surfaced #968 (xargs)
  - #968 → PR #971 → surfaced #970 (env)
  - #970 → PR #974 (env + find sweep) → surfaced #972 + #973
  - #972 + #973 → PR #975 (this release's cap)

  The cascade ended naturally — PR #975's broader test coverage didn't
  surface new gaps. Each linked GitHub issue documents the discovery,
  severity, repro, suggested fix, and test plan.

## [0.2.16] - 2026-04-19

UX-focused release. The TUI now renders syntax highlighting for ~250
languages (TOML, TypeScript, Kotlin, Swift, Lua, …) instead of falling
back to flat white, file paths in tool-call output and transcript
exports are clickable hyperlinks, and the rendering layer is split into
a dedicated `theme` module so future appearance changes ripple through
in one place. No `koda-core` API changes — patch-compatible with v0.2.15.

### Added
- **Clickable file paths in the live TUI.** Every file path styled with
  `theme::PATH` (cyan + underlined) — tool-call headers, `Grep`/`Glob`/
  `List` output, `WebFetch` URLs, markdown links — is now an OSC 8
  hyperlink. ⌘/Ctrl-click to open in iTerm2, Kitty, Wezterm, Ghostty,
  Alacritty, VSCode terminal, Windows Terminal, and any other modern
  emulator. Non-supporting terminals silently swallow the escape
  sequence (per the OSC 8 spec) and just see the underlined text — no
  capability probe needed. New module: `koda-cli/src/hyperlink.rs`.
- **Clickable file paths & URLs in transcript exports.** `/copy` and
  `/export` now emit `Read`/`Write`/`Edit`/`Delete` paths and `WebFetch`
  URLs as markdown links (`[src/main.rs](file:///abs/src/main.rs)`).
  Renders as a clickable link in GitHub, Slack, Notion, VS Code preview,
  iTerm2/Kitty/Wezterm pasted markdown, and any markdown viewer that
  understands the standard. Falls back to readable plain text everywhere
  else. Set `KODA_TRANSCRIPT_HYPERLINKS=off` to disable.
- `tool_header::detail_text(name, args, bash_chars)` — plain-text mirror
  of the existing `detail_spans`. Single source of truth for tool-call
  argument summaries across TUI, history replay, and transcript export.

### Changed
- `transcript::tool_detail_summary` is gone; transcript now delegates the
  per-tool dispatch to the shared `tool_header::detail_text` and only
  layers markdown-link wrapping on top. Closes the third copy of the
  `(name, args) → string` logic deferred from #952.
- Transcript Grep detail now matches the live TUI by quoting the pattern
  (`"TODO" in src` instead of `TODO in src`).

### Removed
- **`koda-ast` and `koda-email` crates deleted entirely.** Both were ghost
  crates: zero `use koda_ast` / `use koda_email` consumers anywhere in the
  workspace, both marked `publish = false` (so the documented
  `cargo install koda-{ast,email}` paths in their READMEs never actually
  worked), neither bundled into any Homebrew bottle or release artifact.
  The original tools that wired them in were removed long ago — `AstAnalysis`
  in #611 and the `EmailRead`/`Send`/`Search` direct-library calls per
  CHANGELOG L466. The standalone MCP binaries had no documented external
  consumers and no real `~/.config/koda/mcp.json` configurations on record.

  Survey of three peer agents (Codex, Claude Code, Gemini CLI) found that
  *none* of them use tree-sitter for syntax highlighting in their TUIs;
  all three buffer-then-highlight code blocks via syntect or highlight.js.
  This invalidated the architectural bet behind the Phase 1 koda-ast
  refactor (#945 / #948) — the eventual tree-sitter Phase 2 was the only
  thing that would have made keeping the crate worthwhile, and the prior
  art said don't build it.

  Net effect: workspace shrinks from 5 → 3 crates (~2500 LoC removed,
  9 tree-sitter grammars + IMAP/SMTP deps gone, two flaky MCP integration
  tests deleted). Per DESIGN.md L71 ("Features built but not used should
  be deleted — git preserves..."). Highlighting work returns to
  `koda-cli/src/highlight.rs` where the existing stateful syntect setup
  already handles streaming correctly. See discussion in #949.

### Changed
- **`koda-ast` refactored into a pure library, MCP server binary removed**
  (#945). The `AstAnalysis` MCP tool was removed from the registry in #611
  and nothing in the workspace consumes the standalone server, so the
  binary, integration test, and `rmcp`/`schemars`/`tracing-subscriber`/
  `tokio` dependencies are gone. Crate now contains only the library:
  - `analysis::*` — file structure summaries, call graphs, post-edit
    `syntax_check` (unchanged behavior).
  - `highlight::*` — **new** language-agnostic semantic-token highlighting
    API (`highlight_spans` returning `Vec<HighlightSpan>` with
    `SemanticToken` classification). Tree-sitter primary backend is
    stubbed for Phase 2; syntect fallback wired now so the public
    surface is stable.
  - `grammar::*` — **new** single-source-of-truth language registry
    (`get_language`, `language_for_extension`, `language_name`)
    extracted from the old `ast.rs`.
  - `tokens::*` — **new** `SemanticToken` enum (Keyword, FunctionCall,
    FunctionDef, Type, String, Comment, …) shared by all renderers.

  This is the foundation for koda-cli consuming AST-grounded syntax
  highlighting (also #945, follow-up PRs). Standalone MCP usage of the
  AST tool was never documented as stable and had no known consumers;
  removing it dropped the workspace dep tree by 4 crates and removed a
  flaky macOS integration test that needed retry logic to stay green.

## [0.2.15] - 2026-04-18

Hotfix release. CI/infra changes only — no runtime, library, or user-facing
behavior changes since v0.2.14. Safe to skip if you don't care about CI noise
levels.

### Changed
- **Coverage workflow distinguishes infra flakes from real regressions**
  (#930, #931). When `taiki-e/install-action` fails to install
  `cargo-llvm-cov` (a known intermittent infra issue), the coverage report
  job no longer treats it as a real coverage drop — it skips opening a
  regression issue and skips updating the badge. Real coverage drops still
  fail loudly. Reduces false-positive regression alerts.
- **Workspace dependency versions synced** so all four crates pin their
  internal `koda-core` dependency to the matching workspace version (#932).
  Bookkeeping for crates.io publishing; no behavior change.

### Coming next
- **v0.3.0 will introduce capability-aware sandboxing** (tracking: #934,
  design discussion: #933). Per-sub-agent filesystem and network policies,
  pre-warmed sandbox slot pool for fast parallel dispatch, and stronger
  isolation than the current `Bash`-only sandbox. Read the design and try
  the alphas if you'd like to influence the implementation.

## [0.2.14] - 2026-04-18

### Fixed
- **MCP server instructions are now injected into the system prompt** (#922,
  #927, #929). Each MCP server can return a free-form `instructions` string
  in its `initialize` response telling the model how to use it best ("prefer
  locator-based queries over CSS selectors", "always use parameterized
  queries", etc). Koda was silently dropping this guidance, leading to
  suboptimal MCP tool usage. The block is composed per-turn (so
  late-connecting servers and `/mcp add` hot-reloads surface in the next
  turn automatically) and wrapped in explicit provenance markers
  (`---[start of server instructions from <name>]---` / `---[end ...]---`)
  so a malicious or compromised server cannot masquerade its output as
  Koda's own behavioral mandates. Zero token cost for users without MCP
  servers configured. Users with MCP servers will see the model start
  following per-server hints automatically with no config changes.
- **`clean_snippet` no longer panics on UTF-8 boundary truncation** in
  email rendering (#917). Previously, snippets that needed to be truncated
  could panic if the cut point fell mid-codepoint (multibyte char).

### Added
- **HTTP client connect + read timeouts** for `build_http_client` (#918).
  Prevents indefinite hangs against slow or hung remote endpoints (web
  fetch, MCP HTTP transport, provider APIs).
- **Two new bundled skills**: `create-agent` and `create-skill` (#919,
  Phase 1 of #850). Walks users through scaffolding a new sub-agent or
  skill with the right metadata, layout, and tone. Try `/skills` to see
  them.

### Changed
- **System prompt is leaner and clearer** as a result of an end-to-end
  audit against Claude Code's prompt structure (#920):
  - Added 5 high-impact behavioral mandates (CC-aligned MUST/NEVER
    instructions around safety, scope creep, and tone) (#924, +231
    tokens). These match the rules already covered prose-style — making
    them mandates makes them harder for the model to soften under load.
  - Removed redundant `### Available Tools` listing (#925, #926, -313
    tokens). All providers (Anthropic, OpenAI-compatible, Gemini)
    already send the full tool schema in the API request body —
    duplicating it in the prompt was strictly redundant content the
    model already had.
  - **Net effect: prompt is 82 tokens *smaller* than v0.2.13 despite
    gaining stronger behavioral coverage.**
- **Built-in `koda_docs` skill renamed to `koda-docs`** for consistency with
  the kebab-case naming used by all on-disk skills (`code-review`, `debug`,
  `simplify`, `remember`, `security-audit`). The skill behavior is unchanged;
  only the identifier passed to `ActivateSkill` differs. Update any custom
  scripts or aliases that reference `koda_docs` by name.

### Security
- **`is_fully_denied` no longer fails open when `HOME` is unset** (#898).
  The in-process check that mirrors the subprocess sandbox previously
  returned `false` for every path when the `HOME` environment variable
  was missing, exposing `~/.config/koda/db` (plaintext API keys) to the
  in-process `Read` tool inside containers, CI environments, or after a
  sandboxed Bash command ran `unset HOME`. Now uses a `HOME` →
  `USERPROFILE` lookup with a path-component fallback that denies any
  path containing `.config/koda/db` consecutively, so the check fails
  *closed* even with no home directory at all. Mitigated by the OS-level
  subprocess sandbox (bwrap / Seatbelt) which independently blocks this,
  but the in-process guardrail is now correct as well.

### Internal
- **`measure_system_prompt` test added** (#921) for prompt-size regression
  tracking. Run with `cargo test -p koda-core --lib measure_system_prompt
  -- --ignored --nocapture` to see a per-section breakdown.
- **Pre-push hook scaled back ~40×** (90s → ~2s) by dropping full
  workspace test runs in favor of `fmt + clippy` (#923). CI still runs
  the full matrix on every PR.
- HTTP-layer test coverage for OpenAI, Anthropic, Gemini providers via a
  new `FakeLlmServer` test fixture (#911, #912, #915, part of #858).
- Lifecycle and projection coverage for `McpManager` (#908) and
  `later_queue` helpers (#910).
- Integration coverage for `/export` file path + verbose vs summary
  modes (#909).
- CI: `lcov` v0-mangling errors bypassed in coverage merge step (#913,
  #916). macOS subprocess pipe race fixed in CI to clear flake-induced
  security advisories.

## [0.2.13] - 2026-04-14

### Added
- **Queue lanes for typing during inference** — users can now type while the
  model is thinking. `Enter` sends mid-turn input (`QueueNext` — steers the
  current turn); `Ctrl+J` defers to a `later_queue` that fires as one
  combined turn after inference completes (#851, #892).
- **Queue preview widget** — a `📋`-prefixed panel above the status bar shows
  up to 3 pending later-queue items with index numbers, an overflow count,
  and keybinding hints (`↑ pop · Ctrl+U clear`). Hidden when the queue is
  empty (#851, #893).
- **Up Arrow pops from later queue** — during inference, pressing `↑` pops
  the last deferred item back into the editor for re-editing (#893).
- **`/export` defaults to verbose** — transcripts now include full tool-call
  output, timestamps, and token counts by default. The new `--summary` flag
  restores the old concise format (#878, #887).
- **Sandbox credential hardening** — 8 new credential directories protected
  from subprocess writes: `.claude`, `.android`, `netlify`, `vercel`, `fly`,
  `doppler`, `stripe`, `heroku`. Linux bwrap integration tests added (#868,
  #879).
- **10 missing docs pages added to `koda_docs` skill index** (#871, #873).

### Changed
- **Context window % shows actual tokens** — the status-bar percentage now
  uses `prompt_tokens` from the provider response instead of the `chars/3.5`
  heuristic, giving accurate readings across all models (#874, #881).
- **Unrestricted file reads** — Read/List/Grep/Glob now work on any path.
  Write/Edit/Delete remain scoped to the project root. The koda database
  (`~/.config/koda/db`) is fully denied for both reads and writes (#876,
  #882).
- **`/help` card improvements** — removed Approval section, added drag & drop
  hint (#870).
- **Reverted supervisor/worker IPC foundation** — the Phase 1 IPC code from
  #884 was reverted cleanly in #890; no residual code remains.

### Fixed
- **Ctrl+C resume broken across all providers** — typing `continue` after an
  interrupted turn sent two consecutive user-side messages to the API, causing
  a 400/422 rejection on every provider. `assemble_messages` now injects a
  synthetic assistant sentinel between any consecutive user-side messages
  at assembly time (never written to DB, disappears after the next real
  reply). Covers both the streaming-interrupted case (`user → user`) and the
  tool-result-interrupted case (`tool → user`) (#875, #886).
- **Incomplete assistant messages poisoning context** — `load_context` now
  filters out assistant messages without `completed_at`, preventing garbled
  DB state from corrupting future turns (#855, #885).
- **Scrolling up during inference snaps back to bottom** — viewport now holds
  position when new tokens arrive while the user is scrolled up (#872).
- **Queue preview hint row clipped** — `height_for` now always includes a
  row for keybinding hints, so `↑ pop · Ctrl+U clear` is visible even with
  1–3 queued items.
- **Newlines in long queue previews** — text is now flattened to spaces
  before truncation, preventing embedded newlines from breaking the widget
  layout.
- **`resolve_path_unrestricted` narrowed to `pub(crate)`** — no external
  callers; reduces public API surface.

### Dependencies
- `rand` 0.9.2 → 0.9.3 (#891).

## [0.2.12] - 2026-04-12

### Added
- **MCP client support** — Koda can now connect to external MCP servers as a
  client, exposing their tools inside the Koda tool registry under the
  `<server>__<tool>` naming convention. Supports both stdio (child-process)
  and Streamable HTTP (MCP 2025-03-26 spec) transports (#855).
- **`/mcp` slash commands** — five new TUI commands: `list`, `add`, `add-http`,
  `reconnect`, `remove`. Hot-reload: servers connect immediately on `add` without
  restarting the session.
- **Tool filtering** — per-server `enabled_tools` allowlist and `disabled_tools`
  denylist; allowlist takes priority when both are set.
- **MCP docs** — new `docs/src/mcp.md` covering all commands, transport options,
  tool naming, filtering, timeouts, and troubleshooting. `/mcp` added to
  `commands.md` and the docs nav (`SUMMARY.md`).

### Fixed
- **SSRF on HTTP MCP transport** — `connect_http()` now validates the target URL
  with `is_safe_url()` before opening any TCP connection, blocking private,
  loopback, and link-local addresses (including `169.254.169.254`).
- **Bearer token log exposure** — `McpTransport` now implements `Debug` manually,
  replacing `bearer_token` values with `[redacted]` so tokens never appear in
  logs or crash dumps.
- **Server name routing collision** — `validate_server_name()` rejects names
  containing `__` (the internal `<server>__<tool>` separator), empty names, and
  names with non-ASCII-alphanumeric characters.
- **Plaintext bearer warning** — `tracing::warn!` emitted when a bearer token is
  sent over `http://` (not `https://`).
- **Sandbox credential reads for CLI tools** — the sandbox now allows read access
  to credential directories needed by common developer CLI tools (`~/.ssh`,
  `~/.aws`, `~/.config/gcloud`, `~/.kube`, `~/.npmrc`, etc.) while still blocking
  writes. Also updated `docs/src/sandbox.md` and `docs/src/approval.md` to
  document the read-allow / write-deny security model (#866).

### Changed
- `rmcp` dependency aligned to `1.4` in `koda-ast` and `koda-email` (was `1.3`,
  resolved to `1.4` via Cargo.lock — now explicit).
- Stale "Phase 3" comment removed from `koda-core/src/mcp/mod.rs`.
- **CI matrix expanded to macOS** — `ci.yml` and `coverage.yml` now run tests on
  both `ubuntu-latest` and `macos-latest`, covering platform-specific code paths
  (seatbelt sandbox, macOS credential rules). Lint (fmt/clippy) stays Linux-only
  to avoid duplicate noise. Coverage merges per-platform lcov traces (#867).

## [0.2.11] - 2026-04-12

### Changed
- **TrustMode replaces ApprovalMode × SandboxMode** — the two-layer
  permission system (Auto/Confirm × None/Project/Strict) is replaced by a
  single `TrustMode` enum with three modes (#855):
  - **Plan** — sandbox on, all writes denied (investigation agents)
  - **Safe** — sandbox on, confirm every side effect (user default)
  - **Auto** — sandbox on, auto-approve all (autonomous coding)
- **Sandbox always active** — kernel sandbox (macOS seatbelt / Linux bwrap)
  with credential protection is always enforced. No more opt-in `--sandbox`
  flag; the sandbox is the safety boundary, not the approval prompt.
- **CLI flag change** — `--sandbox none|project|strict` removed; replaced
  by `--mode safe|auto` (env: `KODA_MODE`). Default is `safe`.
- **Status bar** — shows 📋 Plan, 🔒 Safe, or ⚡ Auto instead of
  approval mode + sandbox mode.
- **Sub-agent trust clamping** — child agents inherit parent's trust mode
  via `TrustMode::clamp()` (never weaker than parent).
- **Auto mode behavior** — destructive operations (Delete, `rm -rf`) are
  now auto-approved in Auto mode. The kernel sandbox enforces the
  perimeter; only writes outside the project root still require
  confirmation.
- **Graceful sandbox fallback** — `sandbox::build()` falls back to
  unsandboxed execution with `tracing::warn!` when the platform backend
  is unavailable (e.g. no `bwrap` on Linux), instead of hard-erroring.
  `bwrap_available()` now probes with a real sandboxed command.
- **Sandbox fallback safety net** — when the sandbox is unavailable and
  trust mode is Auto, mutation/destructive ops downgrade to
  `NeedsConfirmation` so the user still gets a prompt. `From<u8>` for
  TrustMode now fail-safes to Safe instead of Auto (#860).

### Fixed
- **CI sandbox support** — all Linux CI jobs now install bubblewrap and
  enable unprivileged user namespaces via `sysctl` (same approach as
  OpenAI Codex CI). Previously bwrap was installed but couldn't create
  sandboxes due to disabled user namespaces on GH Actions runners.

> **Lineage:** This project continues from [`koda-agent`](https://github.com/lijunzh/koda-agent) (archived at v0.1.5).
> Versions v0.1.0–v0.1.5 of `koda-agent` are documented in that repository's CHANGELOG.

## [0.2.10] - 2026-04-12

### Added
- **Sandbox for Bash tool** — opt-in process sandboxing via `--sandbox project`
  or `--sandbox strict` (env var: `KODA_SANDBOX`).  macOS uses `sandbox-exec`
  (seatbelt profiles, inline via `-p`); Linux uses `bwrap` (bubblewrap).
  Fail-closed: if the backend is unavailable when sandboxing is requested, the
  command fails rather than falling back to unsandboxed (#840, #843).
- **Credential protection (strict mode)** — blocks reads and writes to
  sensitive directories: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.azure`,
  `~/.password-store`, `~/.terraform.d`, `~/.config/gcloud`, `~/.config/gh`,
  `~/.config/op`, `~/.config/helm`, `~/.config/koda/db`; and files:
  `~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
  `~/.docker/config.json`, `~/.vault-token`, `~/.env` (#847, #848).
- **Agent-file write protection** — `.koda/agents/` and `.koda/skills/`
  are write-protected in all sandbox modes to prevent prompt injection
  via sandboxed commands (#849).
- **Sub-agent sandbox inheritance** — child agents inherit the parent's
  sandbox mode via a "never weaken" rule: `parent.stricter(&child)`.  A
  sub-agent can never run with less protection than its caller (#845, #852).
- **Seatbelt profile injection guard** — project root and home paths are
  validated for special characters before interpolation into macOS seatbelt
  profiles, preventing S-expression injection attacks.

### Changed
- **`settings.rs` → `last_provider.rs`** — renamed to accurately reflect the
  module's purpose (last-used provider recall via SQLite KV). No logic
  change (#846).
- **`koda-email` README** — corrected credential storage reference to match
  current architecture (SQLite KV store, not encrypted keystore).

## [0.2.9] - 2026-04-11

### Added
- **Bash safety: token-level classification (shlex + `DangerCheck` enum)** —
  Replaces substring matching with POSIX shell tokenisation via the `shlex`
  crate. `grep "cargo publish" .` and `grep $'cargo publish' .` are now
  correctly `ReadOnly` instead of falsely `Destructive`. Private items
  (`rm`, `sudo`, `git push -f`, `gh pr merge`, …) are expressed as a typed
  `DangerCheck` enum instead of a flat `&[&str]` list, eliminating the whole
  class of quoted-argument false positives (#807, #823, #841).
- **Edit staleness guard** — `edit_file` now computes a SHA-256 of the
  file content on every full read and rejects a subsequent edit if the file
  has changed on disk since the model last read it (external bash tool, the
  user, another agent). Implements the Gemini CLI strategy (#814, #839).
- **Edit multi-match line numbers** — when `old_str` matches more than once,
  the error now lists the exact line numbers so the model can tighten its
  snippet in one shot instead of guessing (#814, #839).
- **Clipboard: OSC 52 fallback for SSH and tmux** — `/copy` and Ctrl+Y now
  work over SSH and inside tmux sessions via OSC 52 terminal escape sequences
  and `tmux load-buffer -w`. Detects SSH via `SSH_CONNECTION` (not `SSH_TTY`,
  which persists in tmux panes after local reattach) (#837).
- **Type-during-inference queue UX** — keystrokes typed while the model is
  streaming are queued and replayed immediately when the response completes;
  the status bar shows pending queue depth (#828).

### Changed
- **Loop detection: Gemini CLI model (feedback injection)** — replaces the
  old windowed-fingerprint hard stop with a two-phase approach: first
  detection injects a "take a step back" system message to nudge the model;
  second detection (model ignored the feedback) hard-stops. Threshold raised
  to 5 consecutive identical calls (matches Gemini CLI's
  `TOOL_CALL_LOOP_THRESHOLD`) (#826, #829, #831, #832).
- **Per-turn tool call deduplication and cap removed** — frontier models
  legitimately emit 30+ parallel tool calls in a single response. Dedup and
  the 20-call cap suppressed valid parallel work. Repeated patterns across
  turns are now the loop guard's responsibility (#831, #832).
- **SSE parser: accept `data:` without trailing space** — the SSE spec
  (RFC) makes the space optional; Gemma 4 and some self-hosted models omit
  it. Fixes frozen inference on affected endpoints (#823, #824).
- **UTC date formatting centralised** — three copies of manual Gregorian
  calendar math replaced with a single `util::utc_now()` backed by the
  `time` v0.3 crate already in the dependency tree (#818, #833).

### Fixed
- **Ctrl+C aborts background HTTP reader** — `SseCollector` now exposes a
  `JoinHandle`; on cancellation the handle is `abort()`ed immediately so the
  TCP connection closes and single-slot servers (LM Studio, vLLM) can accept
  the next request without waiting for the timeout (#825, #827).
- **Duplicate comment removed** — copy-paste artifact in `inference.rs`
  ("Network drop: warning already emitted" appeared twice on consecutive
  lines).

### Internal / CI
- Lint + check merged into the test job — one container, one compile; no
  duplicate build time (#838).
- Test coverage expanded: thinking block persistence, `fmt_age`, tokenizer
  edge cases (CJK paths, trailing backslash, consecutive spaces), image
  rejection heuristics, loop guard repeat count and tool name (#834, #835).
- Behavioral bash-safety tests moved to `koda-core/tests/bash_safety_test.rs`
  (external contract); 20 behavioral + 11 internal = 31 tests (#841).

## [0.2.8] - 2026-04-11

### Added
- **Thinking block persistence** — Claude's `💭 Thinking…` blocks are now
  persisted to the database and rendered on session resume / in transcripts.
  Previously only streamed live and lost on exit (#812).
- **Tool-type-aware output styling** — read-only tool outputs (Read, Grep,
  Glob) render in dim text; mutating tools (Write, Edit, Bash) render in
  bold. Makes scan-reading conversation history much easier (#809).
- **Image rejection warning** — when a model/endpoint doesn't support vision
  input, Koda surfaces an actionable warning ("switch to a vision-capable
  model") instead of silently failing or showing a cryptic API error (#813).
- **Edit staleness last-writer context** — when a stale-file edit fails,
  the error now names the tool that last modified the file and how long ago,
  so the model can self-correct (#815).

### Changed
- **`/copy` repurposed — copies last assistant response to clipboard** — `/copy`
  now copies the Nth-most-recent assistant text response to the system clipboard
  (default `n=1`; `/copy 2` = second-to-last). Reads from the full session DB
  so compacted history is included.
- **`/export [file.md]` — full transcript export** — renamed from the old `/copy`.
  Without an argument, auto-generates a timestamped filename from the first user
  prompt (`koda-YYYYMMDD-HHMMSS-<slug>.md`) and writes to the current directory.
  Explicit path still supported: `/export notes/session.md`. Only relative paths
  accepted (absolute paths and `..` traversal are rejected).
- **Loop detection message** — reworded from misleading "identical arguments —
  rephrase the task" to actionable "called 3× with similar arguments — send a
  follow-up message to continue" (#816).

### Fixed
- **Image paths with spaces** — macOS drag-and-drop paths with backslash-escaped
  spaces (e.g. `Screenshot\ 2026-04-09\ at\ 4.37.01\ PM.png`) are now correctly
  tokenized instead of splitting on spaces (#805).
- **TodoWrite loop detection** — content-aware dedup prevents the model from
  re-emitting identical todo lists that trigger false loop detection. Checkbox
  display upgraded from Unicode circles to `[ ]`/`[→]`/`[x]` (#806).
- **Quoted strings in bash safety check** — patterns like `grep "rm -rf" logs/`
  no longer false-positive as destructive. `strip_quoted_strings()` now also
  handles backslash-escaped quotes inside double-quoted strings (#803, #817).
- **UTF-8 panics in transcript/export** — byte-index slicing (`&s[..77]`) that
  panicked on multi-byte characters (CJK, emoji) replaced with char-safe
  truncation (#817).
- **`is_image_rejection_error` false positives** — bare "vision" substring
  match tightened to require conjunction with support-denial words, preventing
  auth errors from being misclassified (#817).
- **`/export` path traversal** — user-supplied paths are now validated; absolute
  paths and `..` traversal are rejected (#817).

### Removed
- **`Ctrl+Y` (copy last code block) and `Ctrl+U` (copy last response)** — both
  keybindings are removed. Use `/copy` to copy the last response via command.

## [0.2.7] - 2026-04-10

CI-only release. No functional changes to koda-core or koda-cli.

### Fixed
- **crates.io publish pipeline** — `cargo publish -p koda-cli` was failing
  on slow crates.io index days because a fixed `sleep` wasn't enough for
  koda-core to appear in the sparse index before koda-cli tried to resolve
  it as a dependency. Replaced the sleep with a poll loop that queries
  `https://index.crates.io/ko/da/koda-core` directly — the same mechanism
  used internally by cargo-workspaces and cargo-release — and exits as soon
  as the version appears (up to 5 min, matching crates.io's propagation SLO).
  koda-cli has not been successfully published to crates.io since v0.2.2;
  this release establishes a clean, verified baseline.

## [0.2.6] - 2026-04-09

Patch release to fix the v0.2.5 `koda-cli` crates.io publish failure.

### Fixed
- **`koda-cli` build.rs panics during `cargo publish`** — the build script
  read `../docs/src/SUMMARY.md` to embed the user manual at compile time.
  `cargo publish` runs in an isolated sandbox where parent-directory paths
  don't exist, causing a hard panic. This was the root cause behind every
  failed crates.io publish since v0.2.3.

### Changed
- **Self-documentation: embedded manual → URL reference** — the `koda_docs`
  built-in skill no longer bundles the full user manual into the binary
  via `build.rs`. Instead, it provides a URL index pointing to the published
  manual at https://lijunzh.github.io/koda/, and the agent uses `WebFetch`
  to retrieve docs on demand. Benefits:
  - Eliminates `build.rs` entirely (reduced from 87 lines → deleted)
  - Unix platform gate moved to `compile_error!` in `lib.rs` (zero build overhead)
  - Docs are always up-to-date (served from GitHub Pages, not frozen at compile time)
  - No binary bloat (~20 KB of markdown removed from every build)
  - Matches Claude Code's approach (links to `code.claude.com/docs`)
- **koda-ast marked `publish = false`** — koda-ast is a standalone MCP server
  with zero dependents in the workspace. Removed from crates.io publish pipeline,
  GitHub Release binaries, and Homebrew formula. Still built and usable locally.
- **Email tools removed from koda-core** — `EmailRead`, `EmailSend`, and
  `EmailSearch` were hardcoded in koda-core as direct calls to the `koda-email`
  library. This was a DRY violation: the exact same tools exist as a standalone
  MCP server in `koda-email/src/main.rs`. Users who want email can configure
  koda-email as an MCP server instead. This removes the `koda-email` dependency
  from koda-core entirely.
- **koda-email marked `publish = false`** — no longer a crates.io dependency.
  Still builds as a standalone MCP server binary for optional use.
- **Release workflow simplified** — publish chain reduced from 4 crates
  (koda-ast → koda-email → koda-core → koda-cli) to 2
  (koda-core → koda-cli). One fewer 120-second crates.io index wait.

### Architecture note
`koda_docs` remains a **skill** (passive context injection), not an agent
(active sub-process). Skills are zero-cost prompt injection — the right
primitive for "here's where to find information." It stays in **koda-cli**
(product-specific) rather than koda-core (generic engine), using the
existing `inject_builtin_skills()` seam. `build.rs` is fully deleted —
the Unix platform gate uses `#[cfg(not(unix))] compile_error!()` instead.

## [0.2.5] - 2026-04-09

Patch release to fix the v0.2.4 crates.io publish failure for `koda-cli`.

### Fixed
- **`koda-cli` readme path** — `readme = "../README.md"` pointed outside the
  package boundary, causing `cargo publish` to warn/fail. Changed to
  `readme = "README.md"` (the crate's own README) (#797).
- **`koda-core` missing crate metadata** — added `homepage`, `readme`,
  `keywords`, and `categories` fields that were present on `koda-ast` and
  `koda-email` but missing from `koda-core`, risking crates.io rejection (#797).
- **crates.io index propagation** — doubled the inter-publish sleep from 60 s
  to 120 s to give the sparse index enough time to reflect newly published
  dependencies before downstream crates attempt to resolve them (#797).

## [0.2.4] - 2026-04-09

Patch release to correct a broken v0.2.3 crates.io publish.

### Fixed
- **crates.io publish** — v0.2.3 was only partially published: `koda-ast` and
  `koda-email` reached the registry but `koda-core` and `koda-cli` did not,
  leaving the release in an inconsistent state. Root causes:
  - Stale intra-workspace version pin: `koda-core` declared
    `koda-email = { version = "0.2.0" }` while the crate was at `0.2.3`.
    `cargo publish` strips `path` deps and ships only the version constraint,
    causing an ambiguous resolution against the freshly-indexed crate (#794).
  - Missing crate metadata (`repository`, `homepage`, `readme`, `keywords`,
    `categories`, `authors`) on `koda-ast` and `koda-email` (#794).
  - crates.io index propagation window too short (`sleep 30` → `sleep 60`) (#794).
- **Release CI hardening** (#795, #796):
  - `continue-on-error: true` removed from the publish job — a failed publish
    now correctly fails the release workflow instead of going silently green.
  - New `verify-version` gate checks that every intra-workspace `version` pin
    matches the actual crate version, catching stale pins before any build or
    publish step runs.


Skills, tracing, testing, and security hardening release. 42 PRs merged since v0.2.2.

### Removed
- **Windows support dropped** (#788, closes #791) — koda's `Bash` tool
  uses `sh`, which is not a Windows primitive. Supporting Windows properly
  requires a separate PowerShell tool, Windows-native process-group kill
  semantics, and platform-specific path handling throughout — a different
  product. Attempting to build koda on Windows now produces a clear
  compile-time error with a pointer to WSL2.
  **Supported platforms: macOS (x86_64 + arm64) and Linux (x86_64 + arm64).
  Windows users: use WSL2.**

### Added
- **Built-in skills** (#736, #757) — `simplify`, `debug`, `remember` bundled
  skills; offline docs embedded as a skill (guide agent deleted).
- **Skill metadata** (#742, #747) — `when_to_use`, `allowed_tools`,
  `user_invocable`, `argument_hint` fields on `SkillMeta`; surfaced in
  `ListSkills` output.
- **Dynamic system prompt injection** (#744) — skill + agent listings
  injected into the system prompt at inference time (not config load).
- **`allowed_tools` enforcement** (#751) — tool allow-lists enforced at
  the execution layer, not just prompt level.
- **Per-process log files** (#767) — each koda process writes to
  `~/.koda/logs/<pid>.log` with a `latest` symlink.
- **`#[tracing::instrument]`** (#763, #768) — structured spans on
  `inference_loop`, `execute_one_tool`, and `execute_sub_agent`.
- **Golden-file replay** (#776) — `RecordingProvider` / `ReplayProvider`
  in `koda-test-utils` for deterministic conversation replay.
- **`koda-test-utils` crate** (#772, #774) — dedicated test utility crate
  with `Env`, `EnvBuilder`, `insta` snapshot support, and golden-file
  harness.
- **MockProvider call recording** (#770) — `recorded_calls()` and
  `take_env_calls()` for inspecting sub-agent provider traffic.
- **mdBook user manual** (#730) — full user manual at docs site, with
  chapter size gate in CI (#761).
- **Edit placeholder detection** (#708) — rejects `// ... rest of code`
  omission placeholders in `Edit` tool `new_str`.

### Fixed
- **Tracing zero-bytes bug** (#763) — replaced dead `tracing_subscriber`
  filter with `EnvFilter` + `FmtSpan::CLOSE`.
- **Fork bomb classification** (#775) — `:(){ :|:& };:` and variants now
  classified as `Destructive`.
- **Shell output caps** (#710) — enforce collection caps and bump DB
  storage to 2 MB.
- **Agent parity** (#754) — deeper sub-agent prompts, `skip_memory`,
  `allowed_tools` propagation.

### Changed
- **CI coverage gate** raised from 70% → 80% for koda-core + koda-ast (#722).
- **CI consolidation** (#726) — optimized lint/test job dependencies.
- **`pub(crate)` visibility** (#729) — all koda-cli modules scoped to
  crate-internal.
- **Dependency updates** — tokio 1.51.1, fastrand 2.4.1,
  ratatui-textarea 0.9.0, similar 2→3, plus ~30 transitive bumps.

### Testing
- **350+ new tests** across 7 PRs (#709, #711, #712, #714, #718, #720,
  #722, #725, #727) — approval flow, inference loop, DB methods, file
  tools, E2E safety, email module.
- **`insta` snapshot testing** (#774) — snapshot assertions for event
  sequences and config serialization.
- **`EnvBuilder` pattern** (#774) — fluent test setup replacing
  imperative boilerplate.

## [0.2.2] - 2026-04-05

Documentation, safety, and architecture release. 17 PRs merged since v0.2.1.

### Added
- **CLI reference in docs.rs** (#703) — full flag table with env vars
  (`KODA_MODEL`, `KODA_PROVIDER`, `KODA_BASE_URL`) and subcommand docs.
- **Privacy statement** (#703) — explicit zero-telemetry guarantee in
  crate-level docs.
- **Guide agent** (#691) — built-in `/agent guide` for answering questions
  about Koda's features.
- **Registry-driven `/help`** (#684) — `/help` output generated from the
  slash command registry (single source of truth).
- **Tool descriptions with behavioral guidance** (#686) — enriched tool
  descriptions tell the model *when* and *how* to use each tool.
- **Capabilities generated from code** (#685) — tool capability list
  auto-generated from `ToolDef` structs, replacing static markdown.
- **Doc examples on public APIs** (#689) — `cargo test` now exercises
  doc examples for key types.

### Changed
- **All 29 koda-cli modules now `pub`** (#702) — docs.rs renders the full
  user manual. docs.rs is the single source of truth for documentation.
- **Config consolidated into SQLite** (#698) — API keys, settings, and
  last-used provider moved from dotfiles to `~/.config/koda/db/koda.db`.
  Eliminates 3 config files.
- **Parameter name alignment** (#688) — `Grep` and `Glob` tools renamed
  `path` parameter to `file_path` for consistency with Read/Write/Edit.
- **CONTRIBUTING.md merged into CLAUDE.md** (#687) — single contributor
  reference file.
- **README trimmed** (#690, #699) — removed duplicated content, fixed
  outdated claims, kept it scannable.
- **Module docs enriched** (#694, #696) — every module has `//!` docs;
  thin pages expanded with usage examples and cross-references.

### Fixed
- **Destructive commands rejected in headless mode** (#701) — `rm -rf`,
  `sudo`, `git push --force` etc. are now rejected outright when no human
  is present to approve. Previously auto-approved in headless.
- **`ToolEffect` propagated through approval flow** (#701) — approval
  sinks receive the classified effect so they can make policy decisions
  without re-classifying.
- **`blocking_send` panic in tokio runtime** (#701) — replaced with
  `try_send` to avoid panic when called from async context.

## [0.2.1] - 2026-04-04

Bug-fix release. All fixes discovered during real-world usage after v0.2.0.

### Fixed
- **Provider menu label** (#653) — renamed misleading `is_current` marker to
  `key_set`, removed stale provider marker.
- **Context window from local provider API** (#655) — `query_and_apply_capabilities`
  now reads the actual context window from locally-hosted providers (Ollama,
  LM Studio) instead of falling back to the hardcoded lookup table.
- **Graceful degradation for weak models** (#657) — models that can’t
  tool-call no longer crash the inference loop. Falls back to text-only mode
  with a user-visible warning.
- **`full_content` in `load_messages_before`** (#659) — the recall context
  query now includes untruncated tool output, fixing empty results when
  searching older Bash output.
- **Time-based microcompact** (#660) — microcompact no longer clears tool
  results every turn. Now uses message age (5+ minutes) instead of
  per-turn eviction, preserving recent tool output the model needs.

### Changed
- **Auto-compact threshold** (#667) — removed dead `auto_compact_threshold`
  config field (was never read by the inference loop). Hard-coded to 85%
  matching Claude Code’s behavior. The previous hard-coded constant was 90%.

### Testing
- All 288 tests pass across 4 crates
- Clean clippy (zero warnings), clean fmt
- CI: Ubuntu, macOS, Windows

## [0.2.0] - 2026-04-04

Major release closing all P0/P1 architecture gaps vs Claude Code v2.1.88.
50 PRs merged since v0.1.20, touching every layer of the engine.

### Added

#### Streaming Tool Executor (#648)
- **Eager dispatch during streaming** — read-only auto-approved tools execute
  while subsequent tool call arguments are still being streamed from the LLM.
  Overlaps tool execution with generation time (Claude Code's
  `StreamingToolExecutor` pattern).
- `ToolCallReady` variant in `StreamChunk` — emitted by Anthropic parser on
  `content_block_stop`, enabling per-tool completion events during streaming.

#### Tool System Overhaul (#611–#620)
- **AskUser tool** (#615) — model can explicitly request user clarification
  instead of guessing. Read-only, auto-approved.
- **Background Bash** (#616) — `run_in_background: true` parameter spawns
  long-lived processes (dev servers, watchers) without blocking inference.
  Returns PID immediately.
- **WebSearch tool** (#620) — DuckDuckGo search, no API key required.
  Returns title + snippet + URL for top results.
- **Write `overwrite` parameter** (#613) — must be `true` to replace existing
  files. Prevents accidental overwrites.
- **Edit `replace_all` parameter** (#614) — replace all occurrences of
  `old_str` in a single call.
- **Fuzzy `old_str` matching** (#617) — Edit tool suggests closest match when
  exact `old_str` not found (whitespace normalization + similarity scoring).
- **Stale file detection** (#618) — Edit checks file mtime before applying;
  warns if the file changed since the model last read it.
- **Pre-flight input validation** (#612) — tool arguments validated before
  the approval prompt, not after. No more approving a tool call only to get
  a validation error.
- **Parameter rename** (#619) — `path` → `file_path` across all tool schemas
  for consistency. Both names accepted for backward compatibility.

#### Sub-agent System (#631–#635)
- **Built-in sub-agents** (#631) — `task`, `explore`, `plan` agents available
  out of the box.
- **Fork sub-agent** (#632) — sub-agents forked from the parent session
  context, inheriting conversation history.
- **Background sub-agent execution** (#633) — sub-agents run in background
  with progress tracking.
- **Git worktree isolation** (#634) — each sub-agent gets its own git worktree,
  preventing concurrent file conflicts.
- **Verify agent overhaul** (#635) — default-deny write access for verification
  agents. Read-only by default, explicit opt-in for mutations.

#### Context & Compaction (#638–#642)
- **Context analysis struct** (#638) — per-tool token breakdown with duplicate
  file read detection. Enriched context warnings show top token consumers.
- **Microcompact** (#639) — lightweight tool result aging between full
  compactions. Old Read/Bash/Grep results replaced with stubs, no API call.
- **Partial compaction** (#640) — compacts oldest half of context, preserving
  recent messages and original task intent.
- **Streaming Bash output** (#641) — `EngineEvent::ToolProgress` streams
  Bash stdout/stderr line-by-line to the TUI during execution.
- **Bash smart summary** (#642) — large command output stored in full but
  summarized in context (head + tail + line count).

#### Session & Recovery (#622–#626)
- **Network-drop detection** (#622) — `NetworkError` variant distinguishes
  connection drops from clean stream ends. Partial responses discarded.
- **Interrupted turn recovery** (#623) — banner on session resume when the
  previous turn was interrupted.
- **Session titles & mode persistence** (#624) — sessions get auto-generated
  titles; approval mode persisted across resumes.
- **Ctrl+R reverse history search** (#625) — interactive overlay for searching
  command history.
- **Away-summary banner** (#626) — on session resume, shows what happened
  since the user left.

#### Other Features
- **TodoWrite tool + context warning** (#621) — persistent task tracking;
  warning at 80% context usage.
- **Model aliases & /key command** (#630) — `/model` redesigned with alias
  support; `/key` manages API keys interactively.
- **Unified diff preview** (#607) — syntax-highlighted unified diff with
  context lines for Edit/Write previews.

### Changed
- **System prompt** (#602) — aligned with Claude Code conventions for tool
  usage instructions, project context, and behavioral guidelines.
- **Compaction prompt** (#604) — 9-section summarization with `<analysis>`
  scratchpad for higher-quality summaries.
- **Compaction circuit breaker** (#603) — stops retrying after 3 consecutive
  compaction failures.
- **Progressive head-truncation** (#605) — on context overflow, truncates
  oldest messages before falling back to full compaction.
- **Provider descriptions** (#650) — removed inaccurate "Fast inference" and
  "Meta-provider" labels. "Local, no API key" now derived from
  `ProviderType::requires_api_key()`.

### Removed
- **AstAnalysis tool** (#611) — tree-sitter analysis removed from tool
  registry. Post-edit syntax verification was removed in v0.1.18; this
  cleans up the remaining dead tool definition.

### Refactored
- **`tool_dispatch.rs` split** (#643, #646) — 600-line monolith decomposed
  into `tool_dispatch.rs`, `sub_agent_dispatch.rs`, and `approval_flow.rs`.
- **`TurnState` struct** (#645, #647) — 15+ parameters bundled into a single
  struct, threaded through the inference loop.

### Testing
- All 286 tests pass across 4 crates
- Clean clippy (zero warnings), clean fmt
- CI: Ubuntu, macOS, Windows

## [0.1.20] - 2026-03-28

### Added
- **Allow `/tmp`, `$TMPDIR`, and `/dev/*` without confirmation** (#560) —
  temporary directories and device files are now auto-approved in Auto mode,
  removing unnecessary confirmation prompts for common scratch-space operations.

### Fixed
- **Skip paths inside quoted strings in bash path lint** (#562) —
  `lint_bash_paths()` now ignores paths embedded within single- or double-quoted
  strings, eliminating false-positive "outside project" warnings for commands
  like `grep "pattern" /some/path`.

## [0.1.19] - 2026-03-24

### Fixed
- **Filter `<|begin_of_box|>` / `<|end_of_box|>` tokens from streamed output**
  (#550) — these special tokens were leaking into user-visible assistant
  responses. Added to the `SPECIAL_TOKENS` list in `stream_tag_filter.rs` and
  bumped `MAX_TAG_LEN` from 16 to 17. Includes regression tests for
  single-chunk, cross-chunk, and multi-token scenarios.
- **Default `List` tool to non-recursive listing** (#551) — a bare `ls` was
  producing a full recursive project dump. Flipped the `recursive` parameter
  default from `true` to `false` so omitting the parameter gives a shell-style
  top-level listing. Models can still explicitly set `recursive=true`.
- **Normalize tool names from model output to canonical PascalCase** (#548) —
  models sometimes emit tool names in varying cases (e.g. `read`, `READ`,
  `read_file`). Tool dispatch now normalizes to canonical names before lookup.

## [0.1.18] - 2026-03-23

### Fixed
- **Remove post-edit AST syntax verification** (#544) — `verify_syntax_post_edit()`
  no longer runs after Write/Edit. Tree-sitter false positives (for example,
  valid Rust 2024 `&raw[...]`) were worse than no check at all because they
  derailed the model into trying to "fix" correct code. Compiler/runtime checks
  remain the authoritative validation path.
- **Relax insert_message perf threshold on Windows CI** (#543) — avoids flaky
  perf-test failures caused by slower Windows CI timing variance.

## [0.1.17] - 2026-03-22

### Fixed
- **Mouse scroll escape sequences leaking into prompt** (#540) — during
  inference streaming, `tokio::select!` used random fairness, letting engine
  events starve terminal input processing. Mouse escape sequences piled up
  and leaked into the prompt as raw text. Fixed with biased select (terminal
  input first) and batch-draining engine events to reduce redraws.

### Changed
- **Remove hardcoded planning instructions from system prompt** (#530, P3) —
  the Planning section was scaffolding from the Claude 3 Opus era. Modern models
  (Claude 4.x, GPT-4.1, Gemini 2.5) plan natively. Users who need planning
  instructions for weaker local models can add them to `CLAUDE.md`.
- **Context usage via EngineEvent protocol** (#532, P2) — context window
  tracking now flows through `EngineEvent::ContextUsage` instead of global
  `AtomicUsize` statics. The `context` module is now `pub(crate)`. CLI reads
  context percentage from local state updated by events, not from engine globals.
- **DRY: extract `record_tool_result` helper** (#533, P2) — the 5-step
  post-execution sequence (emit result, truncate, persist, track progress,
  track file lifecycle) was duplicated across three execution strategies.
  Now lives in a single function.
- **Clarify "zero IO" in design.md** (#534, P2) — replaced misleading "zero IO"
  with precise statement: zero stdio, direct filesystem access, assumes POSIX.

### Removed
- **Dead code: `ask_continue_or_stop`** (#531, P1) — replaced by async
  `EngineEvent::LoopCapReached` / `EngineCommand::LoopDecision` flow. Zero callers.

## [0.1.16] - 2026-03-20

### Security
- **EmailSend reclassified to LocalMutation** (#525) — previously classified as
  RemoteAction (auto-approved in all modes), EmailSend now requires confirmation
  in Confirm mode to prevent prompt-injection data exfiltration.
- **Bash classifier hardening** (#525) — added interpreter commands (`python -c`,
  `perl -e`, `ruby -e`, `node -e`), nested shells (`sh -c`, `bash -c`),
  `gh api`, `gh auth`, `gh release delete`, and `git clean -f` to
  DANGEROUS_PATTERNS. These bypass vectors now require user confirmation.
- **Bash timeout capped at 300s** (#525) — prevents LLM-controlled DoS via
  arbitrarily large timeout values.
- **DNS rebinding SSRF protection** (#526) — after URL validation passes,
  the hostname is now resolved and all resulting IPs are checked against
  private/internal ranges before the HTTP request is made. Prevents TOCTOU
  attacks where DNS re-resolves to 169.254.169.254 etc.
- **Symlink read bypass** (#526) — `read_file` now canonicalizes the resolved
  path and verifies it still falls within the project root, preventing symlink
  traversal attacks (e.g. `project/link → /etc/passwd`).
- **IMAP search injection** (#529) — user-supplied search queries are now
  sanitized (backslashes and double-quotes escaped) before interpolation
  into IMAP SEARCH commands.

### Fixed
- **Scroll buffer eviction drift** (#528) — `enforce_capacity()` now subtracts
  the visual height (wrapped line count) of each evicted line instead of a flat
  1, preventing scroll position drift with long wrapped lines.
- **`paragraph_scroll()` u16 overflow** (#528) — visual line offset is now
  clamped to `u16::MAX` to prevent silent truncation at narrow terminal widths
  with large buffers.
- **Missing `gh` CLI classifications** (#525) — added `gh repo clone`,
  `gh run watch` (read-only), `gh pr review/comment/close/reopen`,
  `gh issue close/reopen`, `gh release create`, `gh workflow run` (mutation).

### Changed
- **DRY: shared word-wrap algorithm** (#527) — extracted duplicate word-boundary
  wrapping logic from `scroll_buffer` and `wrap_input` into a single
  `wrap_util::visual_line_count()` function.
- **Deduplicate `config_dir`** (#529) — `keystore.rs` now delegates to
  `db::config_dir()` instead of maintaining its own platform-detection code.

## [0.1.15] - 2026-03-18

### Fixed
- **Startup warnings invisible in fullscreen TUI** (#510, #512) — the home-directory
  warning was printed via `eprintln!` before the TUI took over the screen, so users
  never saw it. Warnings are now rendered as styled lines in the TUI scroll buffer,
  consistent with all other startup messages.
- **Empty tool arguments cause JSON parse error** (#513, #514) — when the LLM
  (observed with Anthropic) returned empty or whitespace-only `arguments` for a
  tool call, koda surfaced a raw "Invalid JSON arguments: EOF" error and wasted
  a retry round-trip. Empty args now default to `{}` so tools fall through to
  their own defaults.

## [0.1.14] - 2026-03-18

### Added
- **Post-edit AST syntax verification** (#467, #504) — after Write/Edit, koda-ast
  automatically parses the file with tree-sitter and appends syntax errors to the
  tool result. The LLM gets immediate feedback to self-correct without user
  intervention. Supports Rust, Python, TypeScript, JavaScript, Go, and more.
- **`--resume` CLI flag** (#505, #507) — `--resume <id>` is now the primary flag
  for session resumption (`--session` remains as an alias, `-s` as the short form).
- **Keyboard shortcuts documentation** — user guide now includes a full key binding
  reference table.
- **E2E test suite expansion** (#508) — 13 new end-to-end tests covering Grep,
  Edit, Delete, AST verification, multi-tool execution, and CLI flag regressions.
  Split the 943-line `e2e_test.rs` into 5 focused files (all under 600 lines).

### Fixed
- **`tool_use.input` must be a JSON object** (#501, #502) — Anthropic API returned
  400 errors when the LLM produced empty/null/non-object `arguments`. The provider
  now coerces non-object values to `{}` before sending.
- **Shift+Enter removed, Alt+Enter standardized** (#503, #506) — Shift+Enter was
  unreliable across terminals. Alt+Enter is now the sole newline-insertion key,
  consistently handled in idle, inference, and wizard-input contexts.

### Changed
- **Dependencies** — bumped clap 4.5→4.6, tokio-tungstenite 0.28→0.29,
  unicode-width 0.2.0→0.2.2, tracing-subscriber 0.3.22→0.3.23,
  once_cell 1.21.3→1.21.4, softprops/action-gh-release 2.5→2.6.

## [0.1.13] - 2026-03-17

### Fixed
- **Model output truncated after tool calls** — `CliSink` used `try_send()`
  on a bounded 256-slot channel. When the TUI couldn't drain fast enough
  (e.g., slow terminal redraw during large tool output), `TextDelta` events
  were silently dropped, causing responses to appear cut off (sometimes to a
  single character). Switched to an unbounded channel — the engine produces
  events sequentially and is I/O-bound on LLM streaming, so backlog is
  naturally small.
- **Mouse copy unstable during inference** — the `to_buffer_row` closure
  recalculated scroll position on every Drag event. During inference (sticky
  mode), buffer growth shifted coordinates between MouseDown and MouseUp,
  causing wrong or empty text to be copied. Scroll position is now captured
  once at MouseDown and stored in the `Selection` struct.
- **Scroll position miscalculated** — `scroll_up()` used the full terminal
  height instead of the history area height, both in the idle and inference
  event loops. This caused `max_offset` to be too small.
- **`last_response()` matched markdown HRs** — the separator check accepted
  any line of `─` chars (≥3), including 60-char markdown HRs. Now matches
  exactly 3 `─` characters (the ResponseStart separator).
- **Ctrl+Shift+Y broken on macOS** — most macOS terminals don't distinguish
  Ctrl+Shift+Y from Ctrl+Y. Added **Ctrl+U** as a reliable alternative for
  "copy last response".
- **Drag-selection couldn't cross page boundary** — the `in_history` guard
  on Drag events silently dropped them when the cursor left the viewport.
  Removed the guard; added auto-scroll (1 row/event) at viewport edges.

### Changed
- **Code quality: `tui_context.rs` split** — the 1,355-line god-struct was
  split into `tui_context/{mod.rs, events.rs, menus.rs}` (all under 600
  lines). The `db.rs` monolith was split into `db/{mod.rs, queries.rs,
  tests.rs}`.
- **History navigation refactored** — extracted pure `history_up_index()` /
  `history_down_index()` functions for testability.

### Added
- 10 new unit tests for `tui_context::events` (history persistence,
  truncation, index navigation).
- 8 new unit tests for mouse selection and scroll buffer (cross-page
  selection, rendered content detection, markdown HR regression).
- `cargo audit` verified clean (0 vulnerabilities, 1 allowed warning for
  unmaintained `bincode` via `syntect`).

## [0.1.12] - 2026-03-16

### Fixed
- **Input frozen on launch** — the #458 refactor accidentally dropped the
  `self.draw()` call before the idle `tokio::select!` in the event loop.
  Without it, the viewport never redraws after keystrokes, making the
  textarea appear completely unresponsive. Hotfix release — v0.1.11 is
  unusable.

## [0.1.11] - 2026-03-14

### Fixed
- **Security: drop `sqlx-mysql` transitive dependency** — sqlx default features
  pulled in `sqlx-mysql`, which transitively brought in the `rsa` crate
  (RUSTSEC-2023-0071, timing side-channel). Koda only uses SQLite — set
  `default-features = false` on sqlx, removing `rsa`, `sqlx-mysql`, and ~460
  lines from Cargo.lock. `cargo audit` now shows 0 vulnerabilities (#459)

### Changed
- **Refactor: decompose `inference_loop()`** — extracted token estimation,
  message assembly, overflow detection, and rate-limit helpers into focused
  functions in `inference_helpers.rs` (#455)
- **Refactor: extract `ChunkParser` trait** — unified SSE stream collection
  across all three providers (Anthropic, Gemini, OpenAI-compat) into a shared
  `stream_collector.rs` with per-provider `ChunkParser` implementations (#457)
- **Refactor: extract TUI event loop handlers** — decomposed the 1,200-line
  `tui_context.rs` into `tui_handlers_inference.rs` for inference event
  processing (#458)

### Added
- **Doc-tests** — 7 doc-tests on key pure public APIs: `classify_bash_command`,
  `split_command_segments`, `strip_env_vars`, `mask_key`, `rate_limit_backoff`,
  `truncate_for_display`, `lint_bash_paths` (#461)
- **Dependabot** — enabled vulnerability alerts; created missing `ci`,
  `dependencies`, and `rust` labels for dependabot PRs

### Documentation
- **CLAUDE.md** — rebuilt architecture tree from filesystem (removed 5 stale
  files, added ~15 missing files from v0.1.9–v0.1.10 refactors) (#459)
- **Windows keystore ACL** — documented that `keys.toml` on Windows inherits
  parent directory ACLs (low risk for single-user machines) (#460)

## [0.1.10] - 2026-03-14

### Removed
- **MCP client** — removed the entire MCP client module (`koda-core/src/mcp/`),
  `/mcp` slash command, auto-provisioning, and all integration points.
  First-party tools (koda-ast, koda-email) were already migrated to direct
  library calls in v0.1.9. Standalone MCP server binaries remain for external
  consumers. Net -1,719 lines across 34 files (#443, #444)
- **Capability registry** — removed auto-provisioning of third-party MCP servers.
  Closed #274 (Playwright MCP) as dependent on removed infrastructure
- **`rmcp` dependency** — removed from koda-core (still used by koda-ast and
  koda-email standalone binaries)

### Fixed
- **Stale documentation** — updated CLAUDE.md, DESIGN.md, README.md,
  capabilities.md, and user-guide.md to remove all MCP client references

## [0.1.9] - 2026-03-12

### Added
- **Context Management docs** — new user guide section explaining context loading,
  compaction lifecycle, auto-compaction, purge, and design philosophy
- **`/purge` command** — permanently delete archived (compacted) messages with
  preview stats and y/N confirmation. Supports age filter: `/purge 90d` (#429)
- **Startup nudge** — one-line hint when archived history exceeds 500MB:
  `💡 523MB of archived history — run /purge to clean up`
- **`last_accessed_at` tracking** — sessions table tracks actual activity
  timestamps, enabling age-based purge filtering by real usage
- **`CompactSkip::HistoryTooLarge`** — compaction refuses when history exceeds
  model context window, with clear user-facing warning

### Changed
- **Non-destructive compaction** — `compact_session()` now archives messages
  with `compacted_at` timestamp instead of permanently deleting them.
  Original history preserved in DB for recovery (#428)
- **No sliding window** — `load_context()` loads ALL active (non-compacted)
  messages chronologically. Removed token budgeting, priority-based truncation,
  and `LIMIT 200`. Full model context utilized (#428)
- **Orphan pruning** — `prune_mismatched_tool_calls()` uses symmetric-difference
  of tool_call IDs to drop mismatched tool_use/tool_result pairs. Fixes
  Anthropic API 400 errors from interrupted sessions (#428)
- **No conversation text cap** — removed 20K char cap on compaction
  summarization input. Scales to model capacity instead of hardcoded limit

### Removed
- **Dollar cost estimation** — stripped `cost.rs` (219 lines) and `/cost`
  command. Token counts shown in status bar are sufficient; dollar estimates
  were unreliable across providers (#427)
- **Sliding window** — removed token budgeting, `estimate_tokens()`, and
  priority-based truncation from `load_context()`. Replaced by loading all
  active messages (#428)

### Fixed
- **Orphaned tool_result blocks** — API 400 errors from Anthropic caused by
  mismatched tool_use/tool_result pairs after session interruption or
  compaction boundaries (#428)
- **GitHub Actions Node.js deprecation** — bumped CI/release workflows to
  actions v6 (Node.js 24) (#426)
- **Logs directory** — moved from `.koda_logs/` in project root to
  `~/.config/koda/logs/` (#425)

## [0.1.8] - 2026-03-12

### Added
- **Horizontal overflow indicators** — textarea input shows dim `→` / `←` arrows
  at the edges when content extends beyond the visible width (#416)
- **Paste as collapsible block** — pasted content is inserted as a collapsible
  `<details>` block reference, keeping the input area clean (#244)

### Changed
- **`#![warn(missing_docs)]` on koda-core** — all public items in the engine
  crate now require documentation; ~235 doc comments added (#300)

### Fixed
- **Terminal resize stability** — cursor-aware viewport erase prevents ghost
  prompts and scrollback corruption on resize (#415, #417)
- **Ctrl+L screen refresh** — standard Unix convention cleans up visual
  artifacts from resize reflow; resize warning guides users (#418, #420)
- **Logs moved to `~/.config/koda/logs/`** — no longer creates `.koda_logs/`
  in the project directory

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
- **Alt+Enter** for multi-line input

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
