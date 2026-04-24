# Design — Koda

> Design principles, architecture, and key decisions for contributors
> and maintainers.

---

## Vision

Koda is a personal AI assistant that you own. Not a product from a large
company. Not a platform that locks you into one provider. A tool built for
one person — the author — that works with any API, any local model, and
expands to cover whatever that person needs: coding today, email and
calendar tomorrow, knowledge management next.

The goal is not to build another Claude Code or Cursor. Those are products
designed for broad adoption with configuration surfaces for many users.
Koda is the opposite: hyper-specific, opinionated, and changeable with a
few prompts and a recompile.

What koda specifically rejects from existing tools:

- **Cursor/Windsurf**: Extension and plugin systems, marketplace
  ecosystems, multi-user configuration surfaces. These create complexity
  that serves breadth of adoption, not depth of capability. Koda compiles
  decisions in, it doesn't configure them at runtime (P1)
- **Claude Code/Codex**: Vendor lock-in to a single provider. Tight
  coupling to one company's API means the tool's ceiling is set by that
  company's roadmap. Koda treats providers as interchangeable (P1)
- **Goose**: Stepped wizards and fullscreen forms that obscure the
  conversation. Multi-frontend architecture (Electron, Ink TUI, CLI)
  that adds process management overhead for frontends that don't get
  used. Koda keeps the conversation as the primary surface and ships
  one binary (P2)
- **Aider**: Git-centric workflow that couples the tool to a specific
  development methodology. Koda is a general assistant, not a git
  workflow tool (P3)

These aren't aesthetic preferences — each rejection traces to a
principle.

## Execution Modes

```bash
koda                      # Auto-starts embedded engine + CLI client (default)
koda -p "fix the bug"     # Headless mode (direct engine, no server)
koda server --stdio       # ACP server over stdio (for editor integration)
```

---

## Principles

Principles are truths we enforce on the product, ordered by priority.
They may not be correct for everyone, but we follow them anyway.

### P1: Personal

Koda is built for one person. Not configurable for a group of users —
built for a specific person whose needs can be changed with a few prompts
and a recompile.

- **Any model, any provider.** Cloud APIs, local models, mixed routing.
  No vendor lock-in. The tool serves the person, not the platform
- **Customization over configuration.** If a decision can be made at compile
  time, it must be. Flags that select an execution scenario are fine (`-p`
  for headless, `server --stdio` for ACP) — flags that alter behavior within
  a scenario are not (`--autonomy`, `--model-tier`). If something needs to
  change, change the code
- **Build only what we need.** Code that isn't written has zero bugs.
  Features that were built but aren't used should be deleted — git preserves
  history
- **Delete aggressively.** Carrying dead code forward degrades every future
  decision because it obscures what the system actually does. No
  "extensibility for later" — trait abstractions and plugin systems have a
  cost even when idle

### P2: Simple enough to own alone

One person builds it, one person maintains it, one person debugs it.
This is the principle that prevents koda from becoming Cursor. Cursor is
capable but complex — extension systems, agent modes, model routing
tiers, configuration surfaces. Koda chooses simplicity: fewer concepts,
one binary, match dispatch, two approval modes. We'd rather be slightly
less capable than incomprehensible.

- **Easy to reason about.** If you can't hold the component's behavior in
  your head, it's too complex. Clear component boundaries — engine, UI,
  model, provider — are a consequence of this, not a goal in themselves
- **Make it work first.** Ship working code, refactor to clean design
  second, optimize for performance only when measured. A personal tool
  doesn't need the polish of a product — it needs to work
- **Cohesion over abstraction.** Don't split a file just because it's long.
  Don't add a trait just because you might have a second implementation.
  An 800-line file with one cohesive flow beats two 400-line files that
  require cross-file context-switching

### P3: Build for the world six months from now

AI capabilities are compounding. Design for what models and tools will be
able to do, not what they can do today. Don't build workarounds for current
limitations — they'll be obsolete before they're stable.

- **Let the model drive.** The model plans, reasons, and decides which tools
  to call. The engine executes — it does not reimplement planning,
  verification, or decision-making in application code. Today's model
  limitations are tomorrow's non-issues
- **Don't scaffold around weakness.** Model tiers, capability probes, and
  verbose fallback prompts are building for today. Delete them
- **Frontier models, standard APIs.** Koda targets frontier-class models
  via their standard APIs — whether cloud-hosted (Claude, GPT-4o, Gemini)
  or locally served (Qwen, DeepSeek, Llama via LM Studio/Ollama/vLLM).
  We support the OpenAI-compatible API spec faithfully but do not add
  workarounds for individual models' non-conforming behavior. If a model
  emits malformed output, the fix belongs upstream. See
  [#831](https://github.com/lijunzh/koda/issues/831) for rationale
- **Don't over-compensate for weak models.** If a model emits 66 identical
  tool calls, the answer is "use a better model," not a dedup layer that
  silently papers over the problem. Safety mechanisms should match what
  frontier agents actually ship — not what we imagine might go wrong.
  A code review of Claude Code (zero loop detection), Codex (zero), and
  Gemini CLI (consecutive-call detection + feedback injection) informed
  our approach: consecutive identical calls → feedback injection → hard
  stop only if the model ignores feedback. No windowed fingerprinting,
  no tool-name saturation, no tool-only suppression, no per-turn caps.
  See [#823](https://github.com/lijunzh/koda/issues/823) for the full
  analysis
- **Expand the surface.** Email, calendar, knowledge management — these
  are bets on where AI will be, not where it is

### When principles conflict

P1 says "build only what we need." P3 says "expand the surface." These
can conflict. When they do: P1 wins on **timing** (don't build it yet),
but P3 wins on **architecture** (design so it's easy to add). The
`Persistence` trait exists because P3 says the storage backend will
change — but there's only one implementation because P1 says we don't
need a second one yet.

---

## Architecture

### Engine as a Library, Not a Process (P2, P3)

The engine is a Rust library crate (`koda-core`). It communicates
exclusively through `EngineEvent` (output) and `EngineCommand` (input)
enums — it never touches stdout, stdin, or stderr. Filesystem access
for configuration, memory, and tool execution is direct (assumes POSIX).
See `koda-core/src/engine/event.rs` for the protocol definition.

Studied four projects:
- **xi-editor**: Used stdio JSON-RPC. Discontinued. Lesson: protocol becomes
  bottleneck when core and frontend are separate processes.
- **Zed**: Keeps `agent` (engine) and `agent_ui` (rendering) as separate crates
  in the same binary. Engine has zero UI imports — but it *does* import
  `gpui`, `project`, `fs`, and `language_model`. Zed's "engine" is a
  gpui-flavored library; you cannot run `Thread` outside a `gpui::App`.
- **Goose**: Rust engine + ACP server + multiple frontends (Electron, Ink TUI, CLI).
- **Neovim**: C core + msgpack-RPC. Terminal TUI is just one client.

**Zed's approach inspires; Koda finishes the job**: engine and primary client
in the same binary, server mode optional. But Koda goes one step further than
Zed — `koda-core` has zero IO, all output goes through `EngineSink`, all
input arrives via `EngineCommand` over `mpsc::Receiver`, and `EngineEvent` is
`Serialize`. The proof is the ACP server in `koda-cli/src/server.rs` (~430 LOC)
vs. Zed's equivalent in `zed/crates/agent_servers/src/acp.rs` (~3,467 LOC) —
Koda's smaller because the engine boundary is a serializable enum, not a
bridge between gpui's `Entity`/`Task` world and the wire protocol.

**Concrete consequence**: `KodaAgent` (immutable, `Arc`-shared) is separated
from `KodaSession` (per-conversation, mutable). Zed conflates these into
`Thread`. The split is what makes parallel sub-agents safe: each sub-agent
shares the agent definition by `Arc` and has its own session for state. No
`Arc<RwLock<>>` everywhere.

### ACP (Agent Client Protocol) (P3)

Koda's server mode will speak ACP. Both Zed and Goose independently converged
on ACP (`@agentclientprotocol/sdk`). ACP defines session management, streaming
messages, tool calls with permissions, and status updates — exactly what
Koda needs. Adopting ACP gives us Zed integration for free.

### Extensibility: Thin Core + First-Party Libraries (P1, P2)

The core binary contains only essential tools (file ops, shell, search, web
fetch, memory, agents). Domain-specific capabilities are split into first-party
library crates:

- **First-party libraries** (AST analysis, email): Ship in the same workspace
  as library crates with standalone MCP binary wrappers. `koda-core` calls the
  library functions directly — zero IPC, zero process management. The MCP
  binaries remain available for external consumers (other editors, standalone
  use).

**Principle evolution**: Early v0.1.x compiled everything into one binary.
As the vision expanded beyond coding, we initially adopted MCP for all
domain-specific capabilities ([#113](https://github.com/lijunzh/koda/issues/113)).
In practice, routing in-repo capabilities through stdio JSON-RPC added process
management overhead and IPC failure modes for functionality that ships in the
same workspace. [#431](https://github.com/lijunzh/koda/issues/431) migrated
first-party tools to direct library calls. [#443](https://github.com/lijunzh/koda/issues/443)
removed the then-unused MCP client.

**v0.2.12 re-introduced an MCP client** ([#855](https://github.com/lijunzh/koda/issues/855))
for a fundamentally different purpose: connecting to *third-party* external MCP
servers (Playwright, databases, Slack, user-defined APIs). The MCP client is
exclusively for extending Koda with capabilities outside the workspace.

**Dependency graph**:
```
koda-cli → koda-core  (inference engine + first-party tool calls)
                    → McpManager (optional; connects to third-party MCP servers)
                          → [playwright, db-tools, slack, …] (external, via stdio or HTTP)
```

Koda used to ship two first-party in-repo capability crates (`koda-ast` for
tree-sitter analysis, `koda-email` for IMAP/SMTP). Both were deleted: the
in-tree consumers were removed in earlier refactors (#611 for AstAnalysis,
[CHANGELOG L466] for email tools) and no external consumers ever materialized
for either standalone MCP binary. The graveyard — marked `publish = false`,
not bundled into any release artifact, with no `~/.config/koda/mcp.json` users
on record — was a textbook violation of "features built but not used should be
deleted" (this very document, line 71). Git preserves the work if any of those
capabilities are revived.

**Future first-party capabilities**: should new in-repo capability crates be
added that share the workspace and release cycle, prefer direct library calls
over IPC — simpler, faster, more reliable than stdio JSON-RPC for code that
ships in the same binary anyway. Reserve standalone MCP server binaries for
capabilities that have demonstrated demand from external consumers.

**Server language**: Default to Rust (`cargo binstall`) for koda-maintained
servers. Use Node/Python when critical libraries only exist in those ecosystems.
See [#123](https://github.com/lijunzh/koda/issues/123) for tradeoff analysis.

### Monolithic Database Module (P2)

`db/` stays as a cohesive module. Do not split into sub-modules by domain
(sessions, messages, compaction, metadata). The code is tightly cohesive:
one `Database` struct, one `SqlitePool`, one `impl` block. Splitting into
`db/sessions.rs`, `db/messages.rs`, etc. added boilerplate for zero behavior
change (attempted and reverted in v0.1.2).

**Future trigger**: If v0.2.x adds genuinely new persistence domains (vector
embeddings, knowledge graph, email/calendar), those should be *new files*
alongside `db/` (e.g. `vector_store.rs`), not splits of the existing module.
Split by domain divergence, not by line count.

### Database Backend: SQLite + Persistence Trait (P3)

Keep SQLite for now. The `Persistence` trait lets the backend be swapped
later and enables trait-based testing (mock DB). Cost is minimal (~50 lines).

SQLite is excellent for conversations, sessions, and AST cache. But email,
calendar, documents, and knowledge graphs may require full-text search (FTS5),
vector embeddings, graph relationships, or multi-device sync.

---

## Execution Model

### Async Approval Flow (P3)

Tool approval is an async request/response, not a blocking function call.
In server mode, the approval decision comes from a remote client. The engine
emits `EngineEvent::ApprovalRequest` and awaits
`EngineCommand::ApprovalResponse` — works identically over in-process
channels or network transport.

**Decision variants distinguish the source of "no" (#1022 B15)**.
`ApprovalDecision` has four variants, not three: `Approve`, `Reject`,
`RejectWithFeedback { feedback }`, `RejectAuto { reason }`. The split
between `Reject` (interactive: a human said no) and `RejectAuto`
(structural: no human is in the loop, e.g. headless mode refuses
destructive ops by policy) matters because the model adapts
differently to each. A human "no" is a signal to ask or re-plan; an
auto-reject is a constraint to route around for the rest of the
session. Pre-fix, headless emitted `Reject` for policy-blocked tools,
the model saw `"User rejected this action."`, and would loop asking
the (nonexistent) user for clarification until timeout. Now the model
sees `[auto-rejected: <reason>]` — the same shape as the bg-agent
closed-channel auto-reject (B10), so the "no human, here's why"
signal is uniform across all auto-reject paths. Wire format is
`{"decision":"reject_auto","reason":"…"}` (snake_case, pinned by
`test_reject_auto_wire_tag_is_snake_case`).

### Tool Dispatch: Match Statement, Not Trait Registry (P2)

Tools are dispatched via a `match` statement in `ToolRegistry::execute()`,
not via a `Tool` trait with dynamic dispatch. Rust's exhaustive matching
catches missing tool handlers at compile time — adding a tool without a
match arm is a compile error. A `HashMap<String, Box<dyn Tool>>` would move
this to a runtime error. The match statement works well at the current
scale (~20 tools).

InvokeAgent is handled at the dispatch level (`tool_dispatch.rs`) before
reaching the registry. RecallContext uses an optional `db` + `session_id`
on the ToolRegistry, set via `.with_session()`. No sentinel strings.

**Future trigger**: When tool additions become frequent enough that editing 3
locations per tool (definitions, match arm, module import) is a bottleneck,
convert to a `Tool` trait + `ToolContext`. Do both together, not piecemeal.

### Context Window Auto-Detection (P1, P3)

Context windows are queried from the **provider API** at startup. The
hardcoded lookup table (`model_context.rs`) is the fallback.

Hardcoded values go stale and are wrong for local models where the user
controls context size. LM Studio's `/api/v0/models` reports
`max_context_length`; Gemini's `/v1beta/models/{id}` reports
`inputTokenLimit` and `outputTokenLimit`.

**Precedence**: API value > hardcoded lookup > MIN_CONTEXT (4096).

`query_and_apply_capabilities()` runs in all entry points (TUI, headless,
ACP server, model switch, provider setup).

### Rate Limit Retry (P2)

Exponential backoff retry for 429/rate-limit errors. Up to 5 attempts with
delays of 2, 4, 8, 16, 32 seconds. Long sessions with Opus hit rate limits
regularly. Previously, a 429 killed the session. Now the user sees a
countdown and the request automatically retries.

### Sub-Agent Model Routing (P1, P3)

Sub-agents respect their own provider/model config when explicitly set. The
parent's base_url is only inherited if the sub-agent uses the same provider.

The biggest cost lever — expensive models think, cheap models grunt. A scout
on Gemini Flash costs 1/20th of Opus for codebase exploration. The parent's
Anthropic prompt cache is unaffected because sub-agents make independent API
calls to potentially different providers.

### Sub-Agent Lifecycle (P2, P3)

Four execution modes, one mental model:

1. **Sequential foreground** (`InvokeAgent { prompt }`) — one sub-agent at a
   time, blocks the parent loop until done. Default.
2. **Parallel foreground** (multiple `InvokeAgent` calls in one batch) —
   `tool_dispatch::execute_tools_parallel` runs them concurrently via
   `futures_util::future::join_all`. Each gets its own DB session and (if
   write-capable) its own workspace.
3. **Background fire-and-forget** (`InvokeAgent { prompt, background: true }`)
   — spawned via `tokio::spawn` onto the multi-thread runtime, with the
   resulting `JoinHandle` held by `BgAgentRegistry` as an
   `AbortOnDropHandle`. The registry **lives on `KodaSession`**, not
   inside `inference_loop`, so bg agents survive across turns: the
   model can spawn a long-running explorer in turn 1, return final
   text in the same iteration, and turn 2's first iteration drains
   the result. Owning per-loop would have aborted every still-pending
   task on every turn boundary via `AbortOnDropHandle` (silent data
   loss in the single-iteration response case). The inference loop
   drains completed handles before each iteration via
   `BgAgentRegistry::drain_completed()` and injects results as user
   messages. Bg-spawned agents cannot themselves spawn bg agents
   (override `background: false`). `execute_sub_agent`'s future is
   `Send` by construction — the function returns
   `impl Future<Output = ...> + Send + 'a` rather than using `async fn`,
   forcing the compiler to *prove* Send-ness at the boundary. The
   transitive offender (generic IPC helpers in `koda-sandbox::ipc`) carry
   matching `Send` bounds on their `R`/`W`/`T` parameters; without them,
   `tokio::sync::MutexGuard<WorkerClient>` held across an `await` was
   silently non-Send. **Visibility (#1022 B9)**: bg agents run with
   `engine::sink::BufferingSink`, not `NullSink`. The buffering sink
   captures a narrative trace (one short line per `ToolCallStart`,
   `Info`, auto-rejected approval) capped at 256 lines. The trace
   ships back over the result oneshot as a `BgPayload = (output,
   Vec<String>)` and is surfaced to the user as a multi-line `Info`
   event at result-injection time. `NullSink` is preserved for tests
   and any future fully-detached path. Streaming text
   (`TextDelta`/`TextDone`) is intentionally *not* captured — the
   final output already crosses the oneshot, so capturing would
   duplicate.
4. **Forked context** (`agent_name="fork"`) — copies the parent's full
   conversation history into the new session. Fork children cannot spawn
   sub-agents (recursion guard) — same blanket rule that applies to all
   sub-agents (see invariant below).

**Invariants** — enforced consistently across all four modes:

- **Trust never widens**. `derive_child_trust(parent_runtime, declared)`
  in `koda_core::trust` is the *only* way to compute child trust. Same
  helper for fork, named, and bg paths. **Critical**: `parent_runtime`
  must be the runtime `mode` parameter threaded through dispatch —
  never `parent_config.trust`, which is the *startup* value and is
  not updated by `/safe`/`/auto` toggles (#1022 B19). The structural
  lint test in `koda-cli/tests/regression_test.rs` enforces this at
  CI time.
- **Sandbox never widens**. `compose_child_policy(parent_policy, child_trust,
  root)` calls `SandboxPolicy::compose()` (denies = union, allows = intersect,
  limits = min). Bg agents snapshot the parent's effective policy at spawn
  time — they do not regress to `strict_default()`.
- **Cancellation cascades**. Sub-agents receive `parent_cancel.child_token()`
  via `tokio_util::sync::CancellationToken`. Bg agents are no exception:
  Ctrl+C in the parent kills in-flight bg work. `tokio::task::JoinSet` (or
  `AbortOnDropHandle`) ensures registry drop = task abort = workspace release.
- **Workspace isolation** (P3). Write-capable sub-agents get an isolated
  worktree (macOS: `ClonefileProvider` ~3-4× faster; Linux:
  `GitWorktreeProvider`). Read-only agents share the parent root via
  `CwdProvider`. Parallel write-agents cannot trample each other.
- **Result caching** (P1, cost lever). `SubAgentCache` keyed by
  `(agent_name, prompt_hash)`. Cache hits = no LLM call. Lives on
  `KodaSession` (sibling of `BgAgentRegistry`) so cross-turn hits
  work — the natural "explore X / read result / explore X again"
  follow-up flow doesn't pay LLM cost twice. Invalidated on any
  mutating tool — including mutations performed *inside* a sub-agent
  (sub-agent dispatch routes through `execute_one_tool`, which honors
  `is_mutating_tool` invalidation just like the top-level path).
  Generation-bumping invalidation makes cross-turn safe: stale entries
  become misses without needing explicit eviction.
- **Single tool-dispatch path**. Sub-agent tool execution does *not*
  re-implement approval+execute logic. After the sub-agent's own approval
  check (using its `effective_root` for path-aware decisions), the call
  flows through `tool_dispatch::execute_one_tool` — the same function the
  parent uses. This guarantees two things uniformly: mutating tools
  invalidate `SubAgentCache` regardless of who called them, and Bash
  output streams through the parent's sink. (`InvokeAgent` is the one
  exception — see the no-nesting invariant below.)
- **No nested sub-agents**. Sub-agents (named, fork, *and* background)
  cannot themselves call `InvokeAgent`. Enforced two ways: (1) the tool
  is filtered from every sub-agent's tool definitions so the model never
  sees it; (2) the sub-agent dispatch loop short-circuits with a clear
  refusal message if a rogue or scripted model emits it anyway. The
  parent at depth 0 can fan out as many parallel/background workers as
  it wants — fan-out is what scales delegation, not depth. Allowing real
  recursion was considered and rejected: it requires a depth cap
  (~hundreds of KB of `async fn` state per level), threading `depth: u32`
  through five functions, and matches no real use case anyone has asked
  for. Codex takes the same stance (their sub-agents can't spawn
  sub-agents either). The type-level mutual-recursion cycle between
  `execute_one_tool` and `execute_sub_agent` is broken with `Box::pin`
  so the compiler accepts the call graph; runtime recursion is
  unreachable by construction.
- **Pre-flight validation everywhere**. `tools::validate::validate_tool_call`
  runs *before* approval prompting in the sequential arm (so the user is
  never asked to approve a tool that's guaranteed to fail) and *before*
  execution in the parallel + split-batch parallel + sub-agent arms (where
  every tool was already classified `AutoApprove` and the next step is
  execution). The wrapper `validate_then_execute_one_tool` exists for
  exactly this purpose.
- **No user interaction from sub-agents**. `AskUser` is filtered from
  every sub-agent's tool definitions — both foreground and background.
  Sub-agents are autonomous delegation: the parent gathers any required
  input *before* dispatching, then passes the answer in the prompt. This
  matches Codex / Claude Code / Gemini-CLI's design and avoids the
  multi-sub-agent attribution problem (which sub-agent is asking?) plus
  the bg-agent deadlock problem (no `cmd_rx` reachable from the user).
  As a defense-in-depth backstop, the dummy `cmd_rx` handed to every
  sub-agent is constructed with the sender bound to `_` so it drops
  immediately at construction — if a non-AskUser tool ever hits the
  `request_approval` path inside a sub-agent (e.g. a Destructive
  operation under inherited Plan trust), `recv()` returns `None`
  rather than blocking forever, and the sub-agent loop maps that to a
  clear `[auto-rejected: '<tool>' requires user confirmation but this
  sub-agent has no channel to the user]` tool result. The model can
  reason about and recover from the rejection; the dispatch loop
  continues. This distinguishes from genuine cancellation (Ctrl+C),
  which still surfaces as `[cancelled]`.

**What we considered and rejected**:

- **Codex's collab v2 surface** (`spawn_agent`/`send_input`/`wait_agent`/
  `close_agent`/`resume_agent`, persistent ThreadId-addressed agents,
  inter-agent mailboxes). ~3,000 LOC for a workflow that's rare in personal
  use. P2 says no. The `background: true` + drain-on-next-iter pattern
  covers the same ground in 200 LOC. If the model wants more work it just
  calls `InvokeAgent` again.
- **Codex's `agent_max_depth` + `agent_max_threads` atomic CAS reservations**.
  Koda hardcodes "sub-agents cannot spawn sub-agents" (depth = exactly 1
  for any worker) and adds a per-agent iteration cap. Removes the entire
  class of recursion-depth footguns at zero cost. Revisit only if a real
  use case for depth ≥ 2 ever surfaces — fan-out at depth 1 has covered
  every workflow we've tried.
- **Nested sub-agents** (sub-agent calling `InvokeAgent`). Considered
  during #1022. Tempting because it would let a "manager" sub-agent
  decompose work, but: (a) the master agent can already do that
  decomposition itself; (b) each level costs hundreds of KB of `async
  fn` state plus a workspace, provider, and DB session, so deep stacks
  exhaust the OS thread before the cost dominates; (c) requires depth
  threading through five functions and a hard cap; (d) Codex agrees —
  their sub-agents can't either. Same YAGNI reasoning as `AskUser` from
  sub-agents.
- **Claude Code's seven-typed `Task` taxonomy**
  (`local_bash | local_agent | remote_agent | in_process_teammate | ...`).
  Exactly the "feature surface for many users" P1 rejects. `BgAgentRegistry`
  + `BgRegistry` (for bash) cover the "long-running thing with a result"
  case in 200 lines. Don't grow a `trait Task`.
- **Gemini CLI's LLM-based loop detection** (asks the model "are you stuck?"
  every 10–15 turns at extra token cost). Apex form of the anti-pattern P3
  warns about — "don't scaffold around weakness". Frontier models won't need
  this in six months.
- **Code Puppy's plugin system** (`callbacks.py` with 30+ hooks, agent
  frontmatter that can spawn its own MCP servers). Direct violation of P1
  "customization over configuration". Per-agent MCP = per-agent trust audit.

**What we deferred** (architecture supports it; we'll add when the bug exists):

- **Per-tool `is_concurrency_safe(args)` predicate** (Gemini CLI). Would let
  read-only `Bash` (e.g. `ls`, `cat`) join the parallel batch. Today all
  `Bash` is forced sequential. `bash_safety.rs` already does the analysis
  — surfacing it as a `ToolRegistry` method is ~80 LOC.
- **Sibling-cancel on parallel `Bash` failure** (Claude Code). Per-batch
  child `CancellationToken` + typed `[Cancelled: parallel call X errored]`
  result. ~30 LOC.
- **Streaming tool inputs** (Zed `supports_input_streaming`). High-value
  for `Edit`/`Bash` but requires reworking `ToolRegistry::execute` to take
  a stream. P1 says wait for the bug.
- **`FuturesUnordered` instead of `join_all`** for parallel tool execution
  — emits results as they complete instead of after the slowest. ~20 LOC
  win, no protocol change.

---

## Documentation

### docs.rs as the Documentation Site (P1, P2)

Koda uses `cargo doc` / docs.rs as its documentation site. There is no
separate docs website to build, host, or maintain.

Claude Code hosts a separate docs site at `code.claude.com/docs` and ships
a `claude_code_docs_map.md` index file that the guide agent fetches at
runtime to answer user questions. This requires a docs build pipeline,
hosting infrastructure, and a separate content workflow.

Koda takes a different approach: both `koda-core` (engine library) and
`koda-cli` (TUI, slash commands, REPL) expose their modules as `pub mod`
so that `cargo doc` renders them. The guide agent fetches these pages the
same way CC's guide agent fetches `code.claude.com` — but the content
comes from rustdoc, not a custom site.

This means:

- **Module-level `//!` docs are user-facing documentation**, not just
  developer notes. They should explain *what the feature does and how to
  use it*, not just implementation details.
- **koda-cli modules must be `pub`** even though they have no external
  consumers. This is a deliberate visibility trade for documentation
  coverage — the guide agent needs to read slash command docs, TUI
  keybindings, and onboarding flow.
- **Zero infrastructure.** `cargo doc --open` works offline. docs.rs
  publishes automatically on `cargo publish`. No CI pipeline for docs.
- **Single source of truth.** Docs live next to code, never drift.

---

## Interaction

### No `.koda.md` — Use `CLAUDE.md` (P1, P2)

Koda will NOT introduce a `.koda.md` project rules file. User-authored
project instructions go in `CLAUDE.md`.

Koda already reads `CLAUDE.md` via the `memory.rs` fallback chain
(`MEMORY.md` → `CLAUDE.md` → `AGENTS.md`). Adding `.koda.md` would:
- Create a redundant magic filename with confusing priority semantics
- Force users to maintain two files with overlapping content
- Violate DRY at the ecosystem level — one file should serve both tools

### Conversation-First Interaction (P2)

The conversation is the primary surface. All interactive UI (dropdowns,
approvals, wizards) renders inline within the conversation — never as
fullscreen modals or stepped wizards that obscure the chat history.

*The conversation is the primary surface. Interactions happen within it,
not on top of it.* This is the common thread across Claude Code and Codex.
Goose's stepped wizards and Code Puppy's fullscreen forms violate this —
users find them tedious and disorienting.

**Key choices**:
- Per-command state machine enums, not a generic wizard framework (YAGNI)
- Power-user escape hatch: positional args skip wizards entirely
- No "go back" — Esc to cancel and restart is fine for 2–4 step flows
- Render cache backed by DB — scrollback is a `VecDeque<Line>` viewport
  cache, not a standalone buffer. DB is the source of truth. Virtual scroll
  fetches older messages from DB on demand. No arbitrary buffer cap.
- Single rendering path: all output → render cache → `draw()` (no dual-path)
- Native clipboard (`arboard`) for copy — essential UX since alternate screen
  breaks terminal-native mouse selection for multi-page content

For the viewport layout diagram and interaction patterns, run `/help` in the REPL.

**Competitive analysis**: [#230](https://github.com/lijunzh/koda/issues/230)
**Implementation**: [#229](https://github.com/lijunzh/koda/pull/229), [#472]

[#472]: https://github.com/lijunzh/koda/issues/472

### The Dropdown Is Help (P1, P2)

Removed the `?` keyboard shortcut overlay and `/help` command. The slash
dropdown with descriptions IS the help system.

Three overlapping discovery mechanisms (`?` overlay, `/help` modal, `/`
auto-dropdown) created redundant complexity and viewport resize bugs. The
auto-dropdown on `/` shows all commands with descriptions — that is help.
Keyboard shortcuts moved to the startup banner header.

### Fullscreen Viewport (P2)

Switch from `Viewport::Inline` (terminal-native scrollback) to
`Viewport::Fullscreen` (alternate screen buffer with app-managed scrollback).

Inline viewport had two fundamental bugs that couldn't be fixed within the
inline model:
- **DSR cursor position timeouts** ([#470]): `Viewport::Inline` relies on a
  terminal DSR query to determine cursor position. Some terminals
  (particularly over SSH or slow connections) fail to respond, causing
  multi-second hangs at startup.
- **Resize ghost fragments** ([#418]): When the terminal resizes, inline
  viewport content already "scrolled off" into the terminal's native
  scrollback can't be cleared — causing visual corruption.

**What changed**:
- `scroll_buffer.rs`: app-managed `VecDeque<Line>` with visual-line-aware
  scrolling replaces terminal-native scrollback
- `mouse_select.rs`: click-drag text selection + `arboard` clipboard copy
  (essential — alternate screen disables terminal mouse selection)
- `ansi_parse.rs`: ANSI escape → ratatui Span conversion for tool output
- `history_render.rs`: session history replay into scroll buffer
- Single rendering path: all output → `ScrollBuffer` → `draw_viewport()`

**What didn't change**: The interaction model. Conversation is still the
primary surface. Menus, approvals, and wizards still render in `menu_area`.
The layout is the same — just managed by the app instead of the terminal.

[#470]: https://github.com/lijunzh/koda/issues/470
[#418]: https://github.com/lijunzh/koda/issues/418

---

## Safety

### Folder-Scoped Permissions (P2)

Writes outside `project_root` always require explicit confirmation,
regardless of trust mode. Bash commands are linted for path escapes
before execution.

Defense in depth with three layers — path resolution at execution, path
checks at approval, and heuristic bash linting. The LLM is semi-trusted
(can make mistakes, not adversarial). The concern is accidental blast
radius, not targeted attacks.

For operational details, see the [trust module docs](https://docs.rs/koda-core/latest/koda_core/trust/).

### Security Model (P2)

Per-tool safety classification with three trust modes and hardcoded floors
that override mode settings for high-risk operations.

The LLM is semi-trusted — capable of mistakes, not adversarial. Every tool
call is classified into one of four effects (ReadOnly, LocalMutation,
Destructive, RemoteAction). TrustMode (Plan/Safe/Auto) determines which
effects need confirmation. Hardcoded floors ensure outside-project writes
always require confirmation regardless of mode. The kernel sandbox
(always active) enforces the perimeter.

For trust mode tables, tool effect matrix, and operational details, see
the [trust module docs](https://docs.rs/koda-core/latest/koda_core/trust/).

**Key design choices**:
- Sub-agents inherit the parent’s trust mode (clamped via `TrustMode::clamp()`
  — child can never run with less protection than parent). The clamp is the
  *single source of truth* across all four sub-agent modes (sequential,
  parallel, background, fork) — there is no path that hard-codes a child
  trust value. Same rule for sandbox policy via `SandboxPolicy::compose()`.
- **Kernel-level sandboxing** — always active. macOS uses `sandbox-exec`
  (seatbelt); Linux uses `bwrap` (bubblewrap). Credential directories
  are always protected.

**Accepted risks**:
1. Shell command parsing is heuristic — complex pipelines can bypass classification
2. Network is unrestricted in all modes (required for `cargo fetch`, `npm install`)
3. If the sandbox backend is unavailable (e.g. no bwrap on Linux), Koda falls
   back to unsandboxed execution with a warning rather than hard-erroring

### File Lifecycle Tracking (P2)

Track file create/edit/delete ownership per turn to auto-approve deleting
files that koda created in the same turn.

A common pattern — scaffold a temp file, use it, delete it — requires two
confirmation prompts. The second (“approve delete?”) is redundant when koda
just created the file moments ago. The file tracker (`file_tracker.rs`)
records which files were created/edited per turn. `check_tool()` in
`trust.rs` queries the tracker: if a Delete targets a file koda created
this turn, it’s auto-approved.

**Implementation choices**:
- Paths are canonicalized (`std::fs::canonicalize()` + `path_clean::clean()`
  fallback for not-yet-created files) to prevent `./foo/../bar.txt` vs
  `bar.txt` mismatches
- Success is tracked via `ToolResult.success: bool` (set by
  `ToolRegistry::execute()`) — failed writes don't register ownership
- Tracker resets per turn via `reset()` — no cross-turn state leakage
- In-memory only — no persistence needed since ownership is turn-scoped

---

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
