# Development reference

Reference for working on Koda itself. [`AGENTS.md`](https://github.com/lijunzh/koda/blob/main/AGENTS.md)
holds what an agent needs on every turn; this page holds what it needs only for
a particular job: the full workspace tree, the release and new-capability
checklists, and how CI, the security audit and coverage work.

## Workspace

```
koda/
├── Cargo.toml              # Workspace root
├── apps/desktop/           # The desktop app: Electron and a React page, not a crate (see "The desktop app")
├── koda-core/              # Engine library (zero terminal deps)
│   ├── agents/             # Built-in agent configs (default, explore, plan, verify, task)
│   ├── src/
│   │   ├── lib.rs          # Crate root
│   │   ├── agent/          # KodaAgent (shared config: tools, prompt)
│   │   ├── approval_flow.rs# Approval logic extracted from tool dispatch
│   │   ├── child_agent.rs  # Background sub-agent registry
│   │   ├── compact.rs      # Session compaction (summarize old messages)
│   │   ├── config.rs       # Agent/provider config + provider metadata
│   │   ├── context.rs      # Context window token tracking
│   │   ├── context_analysis.rs # Per-tool token breakdown + duplicate detection
│   │   ├── git.rs          # Read-only git context for the system prompt (branch, default branch, diff, recent commits)
│   │   ├── inference.rs    # Streaming inference loop + tool execution
│   │   ├── inference_helpers.rs # Token estimation, message assembly, overflow detection
│   │   ├── instructions.md # Behavioral instructions in the system prompt
│   │   ├── keystore.rs     # Secure API key storage (SQLite KV store, apikey: prefix)
│   │   ├── last_provider.rs # The model choice earlier versions saved in the database, moved once into config.toml
│   │   ├── loop_guard.rs   # Loop detection + iteration hard-cap
│   │   ├── output_caps.rs  # Output cap scaling based on context window size
│   │   ├── preview.rs      # Pre-confirmation diff previews for Edit/Write
│   │   ├── prompt.rs       # System prompt construction
│   │   ├── runtime_env.rs  # Thread-safe runtime env for API keys
│   │   ├── sandbox.rs      # Process sandbox (macOS seatbelt + Linux bwrap, always-on)
│   │   ├── session.rs      # KodaSession (per-conversation: DB, provider, settings)
│   │   ├── skill_scope.rs  # Skill-scoped tool allow/deny enforcement
│   │   ├── sub_agent_cache.rs # Sub-agent provider/model config cache
│   │   ├── sub_agent_dispatch.rs # Sub-agent invocation dispatch
│   │   ├── tool_dispatch.rs# Tool dispatch — routes tool calls to the registry
│   │   ├── tool_normalize.rs # Tool name normalization (snake_case → PascalCase)
│   │   ├── trust.rs        # TrustMode (Plan/Auto), ToolApproval, check_tool()
│   │   ├── truncate.rs     # Token-safe output truncation
│   │   ├── turn_context.rs # TurnContext and ToolExecutionContext bundles
│   │   ├── version.rs      # Background version checker (queries crates.io)
│   │   ├── engine/         # EngineEvent, EngineCommand, EngineSink trait
│   │   │   ├── mod.rs      # Module root + re-exports
│   │   │   ├── event.rs    # EngineEvent + EngineCommand protocol types
│   │   │   └── sink.rs     # EngineSink trait (how clients receive events)
│   │   └── tools/          # Built-in tools
│   │       ├── mod.rs      # ToolRegistry, ToolEffect, tool definitions
│   │       ├── agent.rs    # Sub-agent invocation tool
│   │       ├── ask_user.rs # AskUser tool (explicit user clarification)
│   │       ├── bg_process.rs # Background process registry + spawning
│   │       ├── cargo.rs    # Cargo tool: structured compiler and test feedback
│   │       ├── catalog.rs  # Read-only tool metadata catalog
│   │       ├── file_tools.rs # Read, Write, Edit, List
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
│   │       └── web_search.rs # WebSearch (Brave, DuckDuckGo)
│   └── tests/              # Engine integration tests
├── koda-cli/               # The `koda` command: arguments, dispatch, koda exec, koda serve, koda mcp
│   ├── src/
│   │   ├── main.rs         # Entry point: the sandbox helpers, then the runtime
│   │   ├── bin/koda_logs.rs # `koda-logs`: read the log database, with filters and --follow
│   │   ├── lib.rs          # Crate root
│   │   ├── app.rs          # Arguments (clap) and dispatch to the TUI, exec, serve and mcp
│   │   ├── headless.rs     # `koda exec`: one prompt, no TUI
│   │   ├── mcp_cmd.rs      # `koda mcp`: add, list and remove MCP servers
│   │   ├── logging.rs      # Where tracing goes: the log database, and the log file if log_dir is set
│   │   └── onboarding.rs   # First run: is there a config yet
│   └── tests/              # Integration tests: the koda binary, its TUI in a pty, koda exec
├── koda-tui/               # The TUI, as Codex's codex-tui
│   ├── src/
│   │   ├── lib.rs          # Crate root; the signal-exit hook koda-cli sets for --oss
│   │   ├── tui_app.rs      # Entry (`run`) and main event loop (ratatui Viewport::Fullscreen)
│   │   ├── child_activity.rs # Live tracker for background-work activity
│   │   ├── clipboard.rs    # Clipboard abstraction
│   │   ├── composer/       # Input box: textarea, paste bursts, history, slash popup (SLASH_COMMANDS)
│   │   ├── frame_requester.rs # Frame draw scheduling
│   │   ├── hyperlink.rs    # OSC 8 hyperlink post-render pass
│   │   ├── inference/      # Inference turn lifecycle: inner event loop + post-turn cleanup
│   │   ├── panic_log.rs    # A panic logged as an error, with its backtrace, and stored at once
│   │   ├── theme.rs        # Semantic color tokens
│   │   ├── tui_context/    # TUI shared mutable state (split into submodules)
│   │   │   ├── mod.rs      # TuiContext struct, event loop, command dispatch
│   │   │   ├── background.rs # /stop, /agents and the overlay's process list
│   │   │   ├── events.rs   # Key/mouse handling, clipboard, history persistence
│   │   │   ├── idle_ui_events.rs # UI events while no turn is running
│   │   │   ├── skills.rs   # /skills through skills/list
│   │   │   └── menus.rs    # Model/provider/session pickers, provider wizard
│   │   ├── tui_render.rs   # EngineEvent → ratatui Line/Span rendering
│   │   ├── tui_commands.rs # Slash command dispatch (/help, /model, /resume, etc.)
│   │   ├── tui_mcp.rs      # /mcp slash commands
│   │   ├── tui_wizards.rs  # Interactive wizards (/provider, /key, /skills)
│   │   ├── tui_output.rs   # Output bridge: scroll_buffer push helpers
│   │   ├── tui_viewport.rs # Fullscreen viewport layout + rendering
│   │   ├── tui_types.rs    # MenuContent, PromptMode, TuiState, shared TUI types
│   │   ├── tool_header.rs  # Tool-call header rendering
│   │   ├── tool_output_lines.rs # Per-line tool output rendering
│   │   ├── scroll_buffer.rs# App-managed scrollback buffer with visual-line scroll
│   │   ├── mouse_select.rs # Mouse text selection + clipboard copy
│   │   ├── ansi_parse.rs   # ANSI escape code → ratatui Span conversion
│   │   ├── md_render.rs    # Streaming markdown → ratatui renderer
│   │   ├── completer.rs    # Tab completion (/commands, @files, /model names)
│   │   ├── file_search.rs  # @ mentions' files, through the server's fuzzyFileSearch
│   │   ├── diff_render.rs  # Diff preview → ratatui renderer (syntax highlighted)
│   │   ├── highlight.rs    # Syntax highlighting via syntect
│   │   ├── startup.rs      # Startup banner rendering
│   │   ├── input.rs        # @file reference processing + image loading
│   │   ├── input_history.rs # ↑ / ↓ history, kept in history.jsonl as Codex's TUI keeps it
│   │   ├── sink.rs         # UiEvent: the engine events the TUI renders
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
│   │       ├── session_menu.rs # Session picker menu
│   │       ├── shortcuts_overlay.rs # Keybinding reference overlay
│   │       └── status_bar.rs# Model, mode, context meter, elapsed time
│   └── tests/              # CLI integration tests
│   └── benches/            # Render benchmark
├── koda-oss/               # koda --oss: llama-server started, shared and stopped with Koda
├── koda-git/               # One hardened way to run git: no hooks, fsmonitor, filters, external diff or textconv
├── koda-protocol/          # The client protocol: JSON-RPC messages and their types
├── koda-client/            # The client API: Koda in-process, typed requests, the event queue
├── koda-server/            # Koda's host: sessions, config, MCP, koda serve --stdio, fuzzyFileSearch
├── koda-config/            # config.toml: reading, checking and editing it, keeping comments
├── koda-providers/         # Model providers (Anthropic, Gemini, OpenAI-compatible) and the model tables
│   ├── src/
│   │   ├── lib.rs          # LlmProvider trait, ChatMessage, ModelSettings, create_provider
│   │   ├── anthropic.rs    # Anthropic Claude (prompt caching, extended thinking)
│   │   ├── error.rs        # Typed provider errors, classified once
│   │   ├── gemini.rs       # Google Gemini (context caching, key-in-URL)
│   │   ├── http.rs         # Provider-neutral HTTP helpers
│   │   ├── openai_compat.rs # OpenAI-compatible (GPT, DeepSeek, local models)
│   │   ├── stream_collector.rs # ChunkParser trait + shared SSE stream collector
│   │   ├── stream_tag_filter.rs # Strip XML tool tags from streaming text
│   │   ├── model_alias.rs  # Curated model aliases (e.g. "sonnet" → exact model ID)
│   │   ├── model_context.rs # Model → context window size lookup table (fallback)
│   │   ├── model_effort.rs # Reasoning effort levels per model
│   │   └── provider_catalog.rs # Static metadata for every supported provider
│   └── tests/              # Each provider's HTTP behaviour against a stub server
├── koda-command/           # Shell command classification (bash_safety.rs, ToolEffect) and scratch paths (scratch_path.rs)
├── koda-mcp/               # MCP client: config, manager, tool bridge
├── koda-skills/            # Skill discovery and activation
│   └── skills/             # Built-in skills (SKILL.md files)
├── koda-memory/            # Instruction files (AGENTS.md, CLAUDE.md) and Koda's notes → first user message
├── koda-state/             # Stored state, apart from the engine
│   └── src/
│       ├── session_store.rs  # SessionStore trait: the session storage contract, and its types
│       ├── db/             # Database: the trait over SQLite (WAL mode), with its context cache and tests
│       ├── sqlite.rs       # How each SQLite file is opened: one spec per file (koda.db, logs_1.db)
│       └── log_db/         # The log database: store, limits, and the tracing layer that fills it
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
model │ auto │ 58% context left │ 🐻 12s  ← status bar
[menu_area]                    ← dropdown / approval / wizard (Min(0))
```

All interactive UI renders in `menu_area`. The history panel fills the
remaining vertical space. Mouse scroll works during inference. Native clipboard
(`arboard`) handles copy since alternate screen disables terminal-native mouse
selection. DESIGN.md (Interaction section) has the interaction system design
and competitive analysis.

While a turn runs, the TUI is a client of Koda's host, as Codex's TUI is of
its app server: it draws the turn from the protocol's notifications (items,
their deltas and notices) and answers the host's requests (approvals,
questions, the loop cap) from its dialogs. [headless.md](headless.md) lists
the messages. See [#70](https://github.com/lijunzh/koda/issues/70) for the
TUI design.

## Engine notes

- **Context from API** (`koda-providers`): queries the actual context window from
  the provider. Fallback: the `model_context.rs` lookup table.
- **Request retry:** four retries with backoff (1/2/4/8s) for 429, 5xx and network errors, or the server's `Retry-After` up to five minutes. Errors are classified once, in `koda-providers/src/error.rs`.
- **Stream retry:** five retries of a reply cut off partway, unless the provider rejected the request.
- **Built-in agents:** `default`, plus the `explore` and `task` sub-agents in
  `koda-core/agents/`, and `fork`. User and project agent configs override them.

### Key types

- **`KodaAgent`** — Shared resources (tools, system prompt). `Arc`-shareable.
- **`KodaSession`** — Per-conversation state (DB, provider, settings). Has `run_turn()`. Koda's host (`koda-server`) owns one per open session and runs its turns, for the TUI and `koda exec` alike.
- **`EngineSink`** — Trait with single method: `fn emit(&self, event: EngineEvent)`. The host's sink turns events into protocol messages and, in process, hands them out as they are for the TUI's renderer.

All providers implement the `LlmProvider` trait (`chat_stream` returning
`Receiver<StreamChunk>`). Tools use PascalCase names.

## Sub-agent approvals

Sub-agents use `check_tool` like the main agent. A call that needs a yes goes
to the session's `approval_flow::Approver` (the server's `HostApprover`), which
asks the client with the agent named and answers on a reply of its own, so
several sub-agents can ask at once and a background agent can ask outside a
turn. Without an approver the call is refused.

In Auto mode destructive ops still confirm because the user said YOLO for normal
work, not for `rm -rf` (#1251).

## Test tiers in detail

Test code lives in `koda-test-utils`, which is never published: the mock
provider (`MockProvider`), the test sink (`TestSink`, one file that koda-core's
own unit tests compile too) and the stub HTTP server. No published crate has a
test feature, so nothing test-only is public on crates.io or in the `koda`
binary, and each crate builds once. A test that runs the `koda` binary, or a
sub-agent the engine builds from its config, scripts the stub server
(`mount_script`, or `ScriptedServer` in a synchronous test) in the mock's
format instead. One script answers requests in the order they arrive, so
agents that call the model at the same time, such as two sub-agents from one
batch of tool calls, each get their own with `mount_script_for`, keyed on how
the agent's system prompt begins (every agent's prompt lists the others, so
matching anywhere in it isn't enough).

**koda-core unit tests** are co-located in `src/` modules, no feature flag:
context tracking, loop detection, config defaults, bash safety, etc.

**koda-core integration tests** are one binary: `tests/all.rs` includes each
file under `tests/suite/` as a module, as Codex's core tests do, so they compile
and link once instead of 40 times. nextest still runs each test in its own
process. A test's name starts with its file, `suite::session_test::…`; run one
file with `cargo nextest run -p koda-core -E 'test(/^suite::session_test::/)'`.

**koda-core E2E tests** (mock provider, CI):

- `tests/suite/e2e_test.rs` — core pipeline: text streaming, tool calls, errors, history, cancel
- `tests/suite/e2e_tools_test.rs` — tool coverage: Glob, Grep, Edit
- `tests/suite/e2e_agent_test.rs` — sub-agent invocation + cache hit
- `tests/suite/e2e_skills_test.rs` — skills, compaction
- `tests/suite/cancel_test.rs` — Ctrl+C interruption during inference

**koda-core integration tests**, no feature flag:

- `tests/suite/file_tools_test.rs` — path safety, Edit
- `tests/suite/guarantee_matrix_test.rs` — trust mode × tool effect matrix
- `tests/suite/inference_recovery_test.rs` — rate-limit retry, context overflow
- `tests/suite/inference_edge_test.rs` — loop detection, empty tool calls
- `tests/suite/e2e_safety_test.rs` — approval gates, bash safety classification
- `tests/suite/session_test.rs` — session lifecycle, TurnStart/TurnEnd events
- `tests/suite/perf_test.rs` — database and path-resolution time bounds
- `tests/suite/tool_wiring_test.rs` — every tool routable + approval-handled
- `tests/suite/tool_normalize_test.rs` — snake_case → PascalCase normalization
- `tests/suite/golden_test.rs` — a scripted conversation replayed from a JSON file
- `tests/suite/snapshot_test.rs` — the Bash classifier's verdict on a fixed set of
  commands, and every provider's metadata. No test pins tool descriptions.

**koda-tui unit tests** are co-located in `src/`: markdown rendering, input
parsing, highlighting.

**koda-cli integration tests** are one binary too, `tests/all.rs` with the
files under `tests/suite/`; run one file with
`cargo nextest run -p koda-cli -E 'test(/^suite::cli_test::/)'`. No feature flag:

- `tests/suite/cli_test.rs` — binary subprocess invocation
- `tests/suite/regression_test.rs` — source-level guards: the TUI's redraw loop, a sub-agent's trust
- `tests/suite/skill_agent_e2e_test.rs` — skill activation + agent switching E2E

**koda-cli smoke tests** (a scripted stub server, CI-safe): `tests/suite/smoke_test.rs` covers
headless mode: the `--json` protocol, approvals by mode, session resume, `--resume`.

**koda-cli stub-server tests** (`FakeLlmServer`, CI-safe):

- `tests/suite/no_summary_compaction_test.rs` — real headless run with the summary call emptied or failed: the task reaches the next request, the run finishes
- `tests/suite/gemini_stub_test.rs` — real headless run on the Gemini provider: a 503, a tool call and a failed reply in one turn
- `tests/suite/anthropic_stream_failure_test.rs` — an `error` event or a missing `message_stop` discards the reply, and its tool calls don't run
- `tests/suite/openai_stream_failure_test.rs` — the same for an OpenAI-compatible server: an error object or a stream with no `finish_reason` or `[DONE]`
- `tests/suite/prompt_text_test.rs` — a multi-line prompt reaches the provider unchanged
- `tests/suite/history_edit_log_test.rs` — a tool result dropped by a repeated call id shows as a `history edited` warning
- `tests/suite/context_count_test.rs` — a reported token count above the estimate triggers compaction; with no count, the estimate decides
- `tests/suite/headless_interrupt_test.rs` — SIGINT during a model call: the run still reports its turn end and summary
- `tests/suite/stop_reason_log_test.rs` — the `model call` line keeps the stop reason when usage follows the finish chunk

## Testing on Linux locally

CI runs every test on Linux too, and it stays the source of truth. A local
Linux machine is for the cases where waiting on CI is slow or blind: a change to
the `bwrap` sandbox, PTY handling, process groups or a `cfg(target_os)` gate,
or a failure that shows up only on Linux CI. Don't make it a step for every
change.

On a Mac, a lightweight VM with its own kernel works; a Docker container doesn't,
because its default seccomp profile blocks the unprivileged user namespaces
`bwrap` needs. With [OrbStack](https://orbstack.dev) (`brew install --cask
orbstack`; [Lima](https://lima-vm.io) is an open-source alternative), create an
Ubuntu machine on the same release as CI's `ubuntu-latest` (26.04 as of
2026-11), then install the build tools and Rust inside it:

```bash
orb create ubuntu:resolute ubuntu
orb -m ubuntu sudo apt-get update
orb -m ubuntu sudo apt-get install -y build-essential pkg-config git bubblewrap
orb -m ubuntu bash -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
orb -m ubuntu bash -lc 'cargo install --locked cargo-nextest'
```

rustup installs the toolchain in `rust-toolchain.toml` on first use. OrbStack's
kernel already allows unprivileged user namespaces, so the `sysctl` lines in
`ci.yml` aren't needed. The machine sees your Mac's files at the same paths,
and `orb` keeps the current directory, so run it from the checkout you're
working in. Keep the build directory on the machine's own disk (faster, and
Linux builds don't mix with macOS ones):

```bash
orb -m ubuntu bash -lc 'CARGO_TARGET_DIR=$HOME/target/koda cargo nextest run --workspace'
```

Pass `-j 4` when local model evals are running, so the build doesn't push their
tasks past their time limits. On an Apple Silicon Mac the machine is arm64 while
CI's runner is x86_64; that has not mattered so far. The first run takes about
five minutes at `-j 4`; later runs build only what changed. To make every
sandbox test run rather than skip, as CI does, create the folders the
credential tests check and set `KODA_REQUIRE_SANDBOX`: a test that can't run
then fails with what it's missing.

```bash
orb -m ubuntu bash -lc 'mkdir -p ~/.ssh ~/.aws ~/.config/koda/db && KODA_REQUIRE_SANDBOX=1 CARGO_TARGET_DIR=$HOME/target/koda cargo nextest run --workspace'
```

## The desktop app

`apps/desktop` is Koda's desktop app: a thin Electron shell around a React
page, a client of the [background server](./background-server.md) like the
TUI. It is not a Cargo crate, so it doesn't touch `cargo build` or crates.io;
it needs Node (the version in `apps/desktop/.node-version`).

```bash
cd apps/desktop
npm ci                    # pinned dependencies (exact versions, save-exact)
npm test                  # unit tests: the socket client, the relay's checks
KODA_BIN=../../target/debug/koda npm test   # also against a real koda serve
KODA_BIN=../../target/debug/koda npm run test:e2e   # the app itself, driven by Playwright
npm run typecheck
npm start                 # build into out/ and open the app
```

- **Shape:** the main process (`src/main/`) owns the connection to the
  server's socket; the page (`src/renderer/`) runs sandboxed, without Node,
  served from `app://koda/` under a strict content policy, and reaches the
  main process only through `src/preload/preload.ts`.
- **The relay** (`src/main/relay.ts`) decides what the page may ask the
  server for. The server treats every client as the user, so a method the
  page doesn't need stays closed, and one that could reach past the session
  (a `baseUrl`, a config, an MCP server's command, an answer to an approval)
  is checked in the main process. A change that opens a method adds its
  check and a test.
- **Types** come from `docs/src/protocol-schema.json`
  (`scripts/gen-types.mjs`, run by `npm test`, `typecheck` and `build`), so
  the page is typed against the protocol of its own commit. They aren't
  committed.
- **The server:** the app starts or joins it with `koda serve --start`, the
  rule `koda` follows (`src/main/connection.ts`): an older server with turns
  running waits for the user's choice in the window; `serve/updating` for a
  newer Koda ends with a "Restart now" banner and no server of the app's own;
  a lost server is retried on the TUI's schedule. The `koda` it runs is
  `KODA_BIN`, else the one inside the app (once packaged), else the first on
  the login shell's `PATH`, which the app reads once (`src/main/shell-path.ts`,
  as Goose's and Hermes's apps do) because an app opened from the Finder has
  launchd's minimal `PATH`. Only `PATH`: keys come from `/key`.
- **Sessions:** the sidebar lists every folder's sessions from
  `session/list` and keeps them current from what the server sends every
  client (`session/started`, `session/name/updated`, `session/deleted`,
  `session/status/changed`). Opening one is `session/resume` with its id
  alone (the relay refuses a `cwd`, `config` or `baseUrl`), so it opens in
  its own folder; the one left is unsubscribed, and a turn running in it goes
  on. Starting a session (a folder from the system's picker) and deleting one
  (after the system's confirmation) are main-process actions
  (`src/main/sessions.ts`), so the page never chooses where commands may
  write. App tests answer those dialogs from the main process with
  `electronApp.evaluate`.
- **The conversation** (`src/renderer/transcript.ts`, pure functions):
  stored turns from `session/resume`, then live events (applied every 50 ms
  so a fast stream doesn't redraw for every word), shown in Claude Code's
  desktop views, which Ctrl+O cycles: **Normal** (lookups one after another
  as one row in the TUI's words, `Read 3 files, searched 5 patterns`; other
  calls with their output folded to 5 lines; edits with their diff; no
  thinking), **Thinking** (the same, with the model's thinking where it
  came) and **Verbose** (every call and all its output). A finished turn's
  steps stay, as in Claude Code; the app doesn't fold the turn as Codex's
  TUI does. A command counts as a lookup by its `commandActions`, whatever
  its effect, as Codex's `Explored` cell. Markdown is react-markdown without
  raw HTML; links open in the browser, images aren't loaded. The prompt box
  sends text only (the relay refuses `mode`, `outputSchema` and non-text
  input for now); Enter while a turn runs steers it, and Stop or Esc
  interrupts.
- **Requests** (`src/renderer/requests.ts`, `RequestCards.tsx`): approvals,
  the model's questions, the loop cap and MCP elicitation show as a card
  above the prompt box with the TUI's choices (yes, yes for the session, no,
  no with what to do instead, Esc to stop or, for a sub-agent, refuse). A
  request answered anywhere is withdrawn with `serverRequest/resolved`.
  The main process sends an answer only when it fits the request
  (`src/main/answers.ts`: no "for the session" unless the request offered
  it), and tells the user through a system notification when the window
  isn't in front and Koda needs them or has finished.
- **Session controls** (`src/renderer/controls.ts`, `SessionBar.tsx`):
  under the title, the model (`model/list` of the session's provider) and
  effort, the mode, the context meter (the TUI's reading: the last call's
  tokens against the window, the 12,000-token baseline left out), Compact,
  and a click on the title to rename. The model's plan
  (`turn/plan/updated`) is a checklist above the prompt box while its turn
  runs; Ctrl+T hides or shows it, as in Claude Code. The relay lets the
  page change the model and effort and lower the mode to Plan; Auto is a
  main-process action (`setMode` in `src/main/sessions.ts`) after a system
  confirmation. A model picked here is the session's; it isn't saved to
  `config.toml`. The bar also forks the session (`session/fork` with its id
  alone: same folder and settings), sets, pauses, resumes and clears its
  goal (`session/goal/*`), and lists what runs in the background
  (`session/backgroundTerminals/list`, read again as items and turns end)
  with Stop for each.
- **Changes** (`src/renderer/diffs.ts`, `ReviewPanel.tsx`): a panel beside
  the conversation with a turn's edits, file by file and numbered: the
  latest `turn/diff/updated` of a turn while the window had the session
  open, else the turn's finished Edit and Write calls' `changes` (which
  have no file lines; the app adds them). A comment on a line is sent with
  the others as one text message (`turn/steer` while a turn runs, else
  `turn/start`), quoting each line with its path and number. Commands' and
  sub-agents' changes aren't there, as the protocol says of the turn diff.
- **Panes** (`src/renderer/Pane.tsx`): each open session is a pane with its
  own transcript, controls and diffs, joined with `session/resume` when the
  pane opens and left with `session/unsubscribe` when it closes or shows
  another session. `App.tsx` holds the panes (up to four), the session
  list and the waiting requests, and hands each batch of events to every
  pane; each state function keeps only its session's events. A session
  is in one pane at most.
- **Packaging** (`npm run package`, Apple Silicon only): `Koda.app` with
  the matching `koda` in `Contents/Resources/bin` (the app uses it first,
  so the app and the server it starts are one version), Electron's fuses
  set (no `ELECTRON_RUN_AS_NODE`, `NODE_OPTIONS` or `--inspect`; the app
  loads only from its asar, whose integrity is checked), and an ad-hoc
  signature (no Developer ID). The release's Mac tarball carries it zipped,
  `Koda.app.zip`, which the formula installs to `share/koda/`: Homebrew
  rewrites the library IDs of the Mach-O files a formula installs
  (`fix_dynamic_linkage`), which breaks the app's signature and fails on
  Electron's `libffmpeg.dylib`, but leaves an archive alone. `koda app`
  unzips it into `~/Applications/Koda.app`, again whenever the version
  there isn't koda's, and opens it; files koda writes aren't quarantined,
  so macOS opens the ad-hoc signed app. Electron's and Chromium's licences
  go in the tarball's `licenses/`. Playwright can't drive the packaged app
  (the fuses turn off the inspector it attaches through): check it by
  starting `Contents/MacOS/Koda` with a scratch `XDG_CONFIG_HOME` and
  reading the page through `--remote-debugging-port`.
- **Tests that start a server** use `koda serve --start` on a scratch
  `XDG_CONFIG_HOME`, never the user's own server. `test:e2e` opens the
  built app with Playwright (`playwright-core`'s Electron driver, unseen,
  `KODA_DESKTOP_TEST=1`); CI runs it under `xvfb-run`. Conversations run on
  a scripted OpenAI-compatible model (`tests/e2e/model.ts`) that the scratch
  home's `config.toml` points `lm-studio` at.
- **Version:** the workspace's (see the release checklist).

## Release checklist

- **First, update the dependencies:** `cargo update`, then the full test suite,
  in a PR of its own. Do it before the release check, so the check measures
  the dependencies that ship. Dependabot updates the crates Koda depends on
  directly, not the ones those pull in, which otherwise drift until an
  advisory fails the audit on release day (`rustix` in 0.6.3).
- Run the release check on that commit, and release the commit it checked.
- Record koda-evals' wire fixture from that commit
  (`bin/capture-koda-wire <koda> <protocol>`, no model, about a minute) and run
  koda-evals' `cargo test`: the same scripted run must grade alike through every
  protocol. A difference is a change in what `koda exec --json` says; in 0.7.13
  it found a sub-agent refusal that had carried no `detail` since 0.7.11.
- Run `scripts/check_codex_parity.sh`: Koda's protocol against Codex's, field by
  field, Codex's experimental methods included (it uses the installed `codex`).
  A difference is renamed to Codex's name or recorded, with its reason, in
  `koda-protocol/codex-deviations.toml`. Run it too when Codex is updated.
- Run koda-evals' `bin/compare-claude-requests <koda>`: Claude Code and Koda on
  one scripted task against a local stub of the Messages API (no model, no key,
  a couple of minutes). Thinking blocks must come back unchanged, each request
  must only add to the one before, and cache breakpoints must stay within four;
  its README lists the differences from Claude Code that are by design. In
  0.7.14 it found `koda exec resume --model` on the wrong provider.
- Move CHANGELOG.md `[Unreleased]` to a versioned section.
- Bump the version in the root `Cargo.toml`: `[workspace.package] version`, which
  every published crate inherits, and the exact pins of the Koda crates under
  `[workspace.dependencies]` (`version = "=0.7.0"`), so a koda-cli always builds
  against the crates it shipped with, whatever later 0.7.x versions exist.
  release.yml's version check reads that file and fails the tag on a mismatch.
  The desktop app carries the same version: `npm version <x.y.z>
  --no-git-tag-version` in `apps/desktop` sets its `package.json` and lock
  file, and its `version.test.ts` fails CI when they differ.
- A 0.7.x release stays additive for users: no config key removed or renamed,
  stored sessions only added to (an older shape still reads), no command or
  flag removed. A release that breaks one of those is the next minor.
- The wire protocol isn't held to that. Koda's clients and server ship together
  and run one version (Lijun, 2026-11-26), so a release may change the protocol;
  the Koda version is the protocol's, as Codex has no separate protocol
  number, and `koda exec --json` readers such as koda-evals pick their reader
  by it. Only what two versions need to update in
  place stays the same: `initialize`'s `serverInfo.version`, `serve/stop` and
  `serve/updating`.
- A new crate inherits the workspace version (`version.workspace = true`), gets a
  pin under `[workspace.dependencies]`, and a `cargo publish --dry-run -p <crate>`
  before the tag. The release publishes every publishable crate with
  `cargo publish --workspace`, in dependency order, and fails if any crate fails
  to publish.
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
11. Update `AGENTS.md` and this page

## Toolchain

The toolchain is pinned in `rust-toolchain.toml` (rustup honors it
automatically). CI installs the same version via an explicit `toolchain:` input
on every `dtolnay/rust-toolchain` step, and ci.yml's "Check toolchain pin" step
fails if any workflow drifts from the file. To bump: edit both in one PR and fix
whatever new lints the new toolchain surfaces.

## CI workflows

Seven workflows live in `.github/workflows/`:

| Workflow | Trigger | Purpose |
|---|---|---|
| `ci.yml` | PRs to `main`, push to `main`, nightly (09:17 UTC) + manual | Gate PRs: format, lint, compile, test, doc, dependency audit |
| `audit.yml` | Weekly (Mon 13:00 UTC) + manual | `cargo audit` of `main`; manages the `security-audit` issue |
| `coverage.yml` | Push to `main`; PRs touching `koda-core/` or `koda-sandbox/`; nightly (09:17 UTC) + manual | Coverage thresholds, with auto issue creation on `main` |
| `release.yml` | Tag `v*` | Version verify, Linux test, cross-platform build, publish, Homebrew |
| `docs.yml` | Push to `main` (docs/ changes) | Build + deploy mdBook to GitHub Pages |
| `codeql.yml` | Push to `main`, weekly (Mon 14:00 UTC) + manual | CodeQL code scanning of Rust and the workflows; not on PRs. Test code (`*/tests/`, `koda-test-utils`) is left out by `.github/codeql/codeql-config.yml` |
| `auto-merge-dependabot.yml` | Dependabot PRs | Enables squash auto-merge; merges once required checks pass |

Release job chain:

```
verify-version ─┬→ audit → test ─┬→ github-release ─┬→ update-homebrew
                └→ build ────────┘                  └→ publish (crates.io)
```

The builds run alongside the tests; nothing ships until both pass.

crates.io and Homebrew are independent distribution channels: both fan out from
`github-release` in parallel. Publishing to crates.io only after `github-release`
ensures binaries are confirmed good before committing to an immutable version
number; it does not need to wait for Homebrew.

### CI jobs (`ci.yml`)

Seven independent jobs run in parallel, one responsibility each, so a red check
names what broke, and an eighth gathers them. Wall-clock is set by the slowest
(Test).

| Job | Check name(s) | Runs |
|---|---|---|
| `fmt` | `Format` | `cargo fmt --check`, plus text guards: tokio test flavor (#1109), toolchain pin, the TUI's core boundary (no `koda_core` in `koda-cli` or `koda-tui`) |
| `clippy` | `Clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `check` | `Check (no features)` | `cargo check --workspace --all-targets` (the crates.io shape) |
| `test` | `Test (ubuntu-latest)`; `Test (macos-latest)` on PRs and nightly | `cargo nextest run --workspace --profile ci`, the doc tests with `cargo test --doc`, then asserts from nextest's JUnit report that the security-boundary tests ran |
| `doc` | `Docs` | `cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-Dwarnings` |
| `audit` | `Security Audit` | `cargo audit --deny unsound --deny yanked`, gated on dependency changes (below) |
| `desktop` | `Desktop app` | The desktop app's type check, unit tests and build; then a debug `koda` build and the app's test against `koda serve`, on a PR only when it changes the app, the protocol, the client or server crates or `ci.yml` |
| `required` | `CI required` | Succeeds only if every job above succeeded |

The pre-push hook (`.githooks/pre-push`) runs only `cargo fmt --check` and
`cargo clippy`, the two cheap checks. CI is the source of truth for the rest.

**Branch ruleset required checks:** `CI required` is the check `main`'s
ruleset should require, as Codex's ruleset requires its `CI required`. What
must pass is then the `required` job's `needs` list, reviewed in `ci.yml`,
rather than a list in the repository's settings; a new job is required once
it's added there. Verify what the ruleset requires with
`gh api repos/lijunzh/koda/rules/branches/main`. A required check that never
reports blocks merging forever, so **never add a workflow-level `paths:` filter
to `ci.yml`**. Skip work inside a job instead, as `audit` does.

**Runner hygiene, in every workflow:**

- **macOS runners, only where needed:** GitHub's macOS arm64 runners are
  short of capacity; Mac jobs have waited 5 to 15 minutes for one, and GitHub
  cancels a job no runner takes in about 15 (0.7.2's Mac build, 2026-11-16).
  `ci.yml` and `coverage.yml` run macOS on PRs and in the nightly run on
  `main`, not on each push to `main`: the PR already ran it, and the nightly
  run checks `main` on macOS and saves the macOS cache PRs restore.
  `release.yml` tests on Linux and builds the Mac binary. A Mac job GitHub
  cancels unstarted can be re-run with `gh run rerun <id> --failed`.
- **Caches:** `Swatinem/rust-cache` saves only on `main`
  (`save-if: github.ref == 'refs/heads/main'`); PRs restore `main`'s cache. A
  PR's own cache can be read only by that PR, and saving seven per PR pushed
  the repository past GitHub's 10 GB cache limit, evicting `main`'s.
- **Timeouts:** every job has `timeout-minutes` well above its usual time;
  GitHub's default is six hours.
- **Pinned actions:** every action is pinned to a commit SHA with its version
  in a comment, and Dependabot's `github-actions` updates bump both.
- **Credentials:** checkouts set `persist-credentials: false`, except the one
  that pushes to the Homebrew tap, so build scripts and tests can't read the
  job's token from `.git/config`.
- **Prebuilt tools:** `cargo-nextest`, `cargo-llvm-cov`, `cargo-audit` and
  `mdbook` come from `taiki-e/install-action`, not `cargo install`.

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
