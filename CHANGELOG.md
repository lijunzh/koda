# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

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
