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
  in the same binary. Engine has zero UI imports.
- **Goose**: Rust engine + ACP server + multiple frontends (Electron, Ink TUI, CLI).
- **Neovim**: C core + msgpack-RPC. Terminal TUI is just one client.

**Zed's approach wins**: engine and primary client in the same binary. Server
mode is optional for external clients.

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
removed the now-unused MCP client entirely.

**Dependency graph**:
```
koda-cli → koda-core  (inference engine + first-party tool calls)
                    → koda-ast   (direct library call from ToolRegistry)
                    → koda-email (direct library call from ToolRegistry)

koda-ast/main.rs  → standalone MCP server (for external consumers)
koda-email/main.rs → standalone MCP server (for external consumers)
```

For in-repo capabilities that share the same workspace and release cycle,
direct library calls are simpler, faster, and more reliable than IPC. The
standalone MCP binaries keep each domain independently testable and usable
by external consumers.

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

For the viewport layout diagram and interaction patterns, see
[user-guide.md](user-guide.md#slash-commands).

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
regardless of approval mode. Bash commands are linted for path escapes
before execution.

Defense in depth with three layers — path resolution at execution, path
checks at approval, and heuristic bash linting. The LLM is semi-trusted
(can make mistakes, not adversarial). The concern is accidental blast
radius, not targeted attacks.

For operational details, see [user-guide.md](user-guide.md#security-model).

### Security Model (P2)

Per-tool safety classification with two approval modes and hardcoded floors
that override mode settings for high-risk operations.

The LLM is semi-trusted — capable of mistakes, not adversarial. Every tool
call is classified into one of four effects (ReadOnly, LocalMutation,
Destructive, RemoteAction). Approval modes (Auto/Confirm) determine which
effects need confirmation. Hardcoded floors ensure destructive operations
and outside-project writes always require confirmation regardless of mode.

For approval mode tables, tool effect matrix, and operational details, see
[user-guide.md](user-guide.md#security-model).

**Key design choices**:
- Sub-agents inherit the parent's approval mode (clamped — Auto parent still
  gets Confirm child if the child is set to Confirm)
- No kernel-level sandboxing yet — seccomp/landlock is a v1.0 concern

**Accepted risks**:
1. No kernel-level sandboxing — in-process only
2. Shell command parsing is heuristic — complex pipelines can bypass
3. Outside-project writes in Confirm mode show confirm prompt instead of clean block

### File Lifecycle Tracking (P2)

Track file create/edit/delete ownership per turn to auto-approve deleting
files that koda created in the same turn.

A common pattern — scaffold a temp file, use it, delete it — requires two
confirmation prompts. The second ("approve delete?") is redundant when koda
just created the file moments ago. The file tracker (`file_tracker.rs`)
records which files were created/edited per turn. `check_tool()` in
`approval.rs` queries the tracker: if a Delete targets a file koda created
this turn, it's auto-approved.

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
