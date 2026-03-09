# Koda Design Decisions

This document captures architectural decisions and their rationale.
For the TUI architecture (v0.1.2), see [#70](https://github.com/lijunzh/koda/issues/70).
For workspace structure and developer docs, see [CLAUDE.md](CLAUDE.md).

## Vision

Koda is a personal AI assistant. Coding is the starting point, but the platform
will expand to support email, messaging, calendar, reminders, documentation,
and knowledge management — all powered by the same engine.

## Design Principles

**Adapt to behavior, not configuration.** AI should learn human intention from
historical interaction patterns, not from config files or mode flags. The system
observes how the user intervenes (or doesn't) at each decision point and adjusts
autonomy accordingly. Configuration is a confession that the system can't figure
it out — a personal AI tool should learn how you work by working with you.

This principle drives several architectural choices:
- `TierObserver` learns model capability from tool-use quality, not model names.
- `InterventionObserver` ([#242](https://github.com/lijunzh/koda/issues/242))
  will learn human oversight preferences from phase-gate override patterns.
- No `DepthMode` enum or `--autonomy` flag — autonomy is a continuous variable
  that emerges from data, not a discrete setting the user picks.

## Execution Modes

```bash
koda                      # Auto-starts embedded engine + CLI client (default)
koda -p "fix the bug"     # Headless mode (direct engine, no server)
koda server --stdio       # ACP server over stdio (for editor integration)
```

## Design Decisions

### 1. Engine as a Library, Not a Process

**Decision**: The engine is a Rust library crate (`koda-core`) with zero IO.
It communicates exclusively through `EngineEvent` (output) and `EngineCommand`
(input) enums. See `koda-core/src/engine/event.rs` for the protocol definition.

**Rationale**: Studied four projects:
- **xi-editor**: Used stdio JSON-RPC. Discontinued. Lesson: protocol becomes
  bottleneck when core and frontend are separate processes.
- **Zed**: Keeps `agent` (engine) and `agent_ui` (rendering) as separate crates
  in the same binary. Engine has zero UI imports.
- **Goose**: Rust engine + ACP server + multiple frontends (Electron, Ink TUI, CLI).
- **Neovim**: C core + msgpack-RPC. Terminal TUI is just one client.

**Zed's approach wins**: engine and primary client in the same binary. Server
mode is optional for external clients.

### 2. ACP (Agent Client Protocol)

**Decision**: Koda's server mode will speak ACP.

**Rationale**: Both Zed and Goose independently converged on ACP
(`@agentclientprotocol/sdk`). ACP defines session management, streaming
messages, tool calls with permissions, and status updates — exactly what
Koda needs. Adopting ACP gives us Zed integration for free.

### 3. Extensibility: Thin Core + Auto-Provisioned MCP

**Decision**: The core binary contains only essential tools (file ops, shell,
search, web fetch, memory, agents). Domain-specific capabilities (AST analysis,
email, calendar, browser) are delivered as MCP servers, auto-installed on demand.

**Principle evolution**: Early v0.1.x compiled everything into one binary.
As the vision expanded beyond coding, we realized "everything just works"
doesn't require "everything compiled in" — it requires "everything
auto-provisioned with zero user config." The user experience is identical —
zero friction — but the implementation scales to domains beyond coding.

**How it works**: When the LLM calls a tool that isn't built-in, koda checks
an MCP capability registry, auto-installs the matching server, connects it,
and retries — transparently. The user sees a brief spinner on first use;
subsequent calls are instant.

**Rationale**: As koda expands to email, calendar, and knowledge management,
compiling every integration into one binary creates bloat. The MCP protocol
is the contract; the implementation language and deployment model are details.
AST analysis is the pilot for this pattern (see [#113](https://github.com/lijunzh/koda/issues/113)).

**MCP server language**: Default to Rust (`cargo binstall`) for koda-maintained
servers. Use Node/Python when critical libraries only exist in those ecosystems.
See [#123](https://github.com/lijunzh/koda/issues/123) for tradeoff analysis.

### 4. Async Approval Flow

**Decision**: Tool approval is an async request/response, not a blocking
function call.

**Rationale**: In server mode, the approval decision comes from a remote
client. The engine emits `EngineEvent::ApprovalRequest` and awaits
`EngineCommand::ApprovalResponse` — works identically over in-process
channels or network transport.

### 5. Database as a Monolithic Module

**Decision**: `db.rs` (~1,300 lines) stays as a single file. Do not split
into sub-modules by domain (sessions, messages, compaction, metadata).

**Rationale**: Attempted and reverted in v0.1.2. The code is tightly cohesive:
one `Database` struct, one `SqlitePool`, one `impl` block. Splitting into
`db/sessions.rs`, `db/messages.rs`, etc. added `use super::{Database, MessageRow,
Role, ...}` boilerplate to every sub-file for zero behavior change. The types,
queries, and row conversions are coupled by design (SQLite access patterns).

**Future trigger**: If v0.2.x adds genuinely new persistence domains (vector
embeddings, knowledge graph, email/calendar), those should be *new files*
alongside `db.rs` (e.g. `vector_store.rs`), not splits of the existing module.
Split by domain divergence, not by line count.

### 6. Database Backend Evolution

**Decision**: Keep SQLite for now. Introduce a `Persistence` trait so the
backend can be swapped later.

**Rationale**: SQLite is excellent for conversations, sessions, and AST cache.
But email, calendar, documents, and knowledge graphs may require full-text
search (FTS5), vector embeddings, graph relationships, or multi-device sync.
The trait boundary lets us evolve without rewriting.

### 7. Tool Dispatch: Match Statement, Not Trait Registry

**Decision**: Tools are dispatched via a `match` statement in `ToolRegistry::execute()`,
not via a `Tool` trait with dynamic dispatch.

**Rationale**: Rust's exhaustive matching catches missing tool handlers at compile
time — adding a tool without a match arm is a compile error. A `HashMap<String, Box<dyn Tool>>`
would move this to a runtime error. The match statement works well at the current
scale (~20 tools).

**v0.1.3 update**: The `__INVOKE_AGENT__` sentinel was removed. InvokeAgent is now
handled at the dispatch level (`tool_dispatch.rs`) before reaching the registry.
RecallContext uses an optional `db` + `session_id` on the ToolRegistry, set via
`.with_session()`. No more sentinel strings anywhere.

**Future trigger**: When tool additions become frequent enough that editing 3
locations per tool (definitions, match arm, module import) is a bottleneck,
convert to a `Tool` trait + `ToolContext`. Do both together, not piecemeal.

### 8. Model-Adaptive Architecture (v0.1.3 → v0.1.4)

**Decision**: Koda uses three prompt tiers (Strong/Standard/Lite) and adapts
them at runtime based on **observed tool-use quality**, not model names.

**Rationale**: Name-based detection was fundamentally broken — a 122B MoE model
on LM Studio would get Lite tier, GPT-4o-mini would get Strong, and any new
model would be wrong until the hardcoded list was updated. No metadata signal
(name, param count, context size, provider) reliably predicts tool-use ability.

**How it works**:
- All models start at **Standard** tier
- `TierObserver` tracks tool call outcomes (valid / unknown name / malformed args)
- After 3 successful turns → **promote to Strong** (terse prompt, lazy tools)
- After 2+ hallucinated names or malformed args → **demote to Lite** (verbose prompt)
- Tier transitions are applied at compaction boundaries (prompt is rebuilt anyway)
- CLI `--model-tier` flag and agent JSON `"model_tier"` override the observer

**Resource limits are decoupled from tiers**: iteration cap (200), parallel tools
(always on), and auto-compact threshold (85%) are the same for all tiers. Tiers
only control **prompt strategy** (verbosity + tool loading).

**Key constraint**: System prompt must be stable within a session for Anthropic
prompt cache hit rates. Tier changes are queued and applied at compaction.

### 9. Context Window Auto-Detection (v0.1.3 → v0.1.4)

**Decision**: Context windows are queried from the **provider API** at startup.
The hardcoded lookup table (`model_context.rs`) is the fallback.

**Rationale**: Hardcoded values go stale and are wrong for local models where
the user controls context size. LM Studio’s `/api/v0/models` reports
`max_context_length`; Gemini’s `/v1beta/models/{id}` reports `inputTokenLimit`
and `outputTokenLimit`.

**Precedence**: API value > hardcoded lookup > MIN_CONTEXT (4096).

**Called everywhere**: `query_and_apply_capabilities()` runs in all entry
points (TUI, headless, ACP server, model switch, provider setup).

### 10. Lazy Tool Loading with DiscoverTools (v0.1.3)

**Decision**: Strong-tier models get only 9 tools (8 core + DiscoverTools)
upfront. Everything else is discoverable on demand by category.

**Rationale**: 20+ tool schemas cost ~2000 tokens/turn. Core tools (Read, Write,
Edit, etc.) handle 90%+ of turns. Agents, skills, memory, web, AST, and email
tools are situational. DiscoverTools costs ~50 tokens for the schema + ~80 tokens
for category hints in the system prompt.

**Net savings**: ~57% reduction in per-turn tool overhead for Strong tier.
Standard and Lite tiers still get all tools (they need the explicit schemas).

### 11. Rate Limit Retry (v0.1.3)

**Decision**: Exponential backoff retry for 429/rate-limit errors. Up to 5
attempts with delays of 2, 4, 8, 16, 32 seconds.

**Rationale**: Long sessions with Opus hit rate limits regularly. Previously,
a 429 killed the session. Now the user sees a countdown and the request
automatically retries.

### 12. Sub-Agent Model Routing (v0.1.3)

**Decision**: Sub-agents respect their own provider/model config when
explicitly set. The parent's base_url is only inherited if the sub-agent
uses the same provider.

**Rationale**: The biggest cost lever — expensive models think, cheap models
grunt. A scout on Gemini Flash costs 1/20th of Opus for codebase exploration.
The parent's Anthropic prompt cache is unaffected because sub-agents make
independent API calls to potentially different providers.

### 13. No `.koda.md` — Use `CLAUDE.md` (v0.1.4)

**Decision**: Koda will NOT introduce a `.koda.md` project rules file.
User-authored project instructions go in `CLAUDE.md`.

**Context**: Issue #219 proposed a `.koda.md` file for user-authored project
rules, separate from the LLM-authored `MEMORY.md`. The idea was borrowed from
`.cursorrules` and Claude Cowork's per-folder instructions.

**Why not**: Koda already reads `CLAUDE.md` via the `memory.rs` fallback chain
(`MEMORY.md` → `CLAUDE.md` → `AGENTS.md`). Adding `.koda.md` would:
- Create a redundant magic filename with confusing priority semantics
- Force users to maintain two files (`CLAUDE.md` for Claude Code, `.koda.md`
  for Koda) with overlapping content
- Violate DRY at the ecosystem level — one file should serve both tools

**What to do instead**: Put project rules in `CLAUDE.md`. It's already
loaded into the system prompt, already version-controlled, and already
compatible with Claude Code. Teams using both tools get one file, not two.

**Global config** (`~/.koda/config.toml` for default provider, model,
approval mode) remains a valid future feature but is orthogonal to project
rules and should be tracked separately.

### 14. Interaction System — Inline, Never Fullscreen (v0.1.4)

**Decision**: All interactive UI (dropdowns, approvals, wizards) renders in a
fixed `menu_area` below the status bar inside the ratatui viewport. The
conversation is always visible. No fullscreen takeover, ever.

**Principle**: *The conversation is the primary surface. Interactions happen
within it, not on top of it.* This is the common thread across Claude Code
and Codex. Goose's stepped wizards and Code Puppy's fullscreen forms violate
this — users find them tedious and disorienting.

**Viewport layout** (established in [#229](https://github.com/lijunzh/koda/pull/229)):
```
[LLM output in terminal scrollback]   ← always visible
─── 🐻 ─
⚡> input                              ← fixed, never moves
────────────────────────────────
model │ auto │ 0%                      ← fixed, hugs input
  [menu_area]                          ← dropdown / approval / wizard
  [empty when inactive]                ← looks like terminal bottom
```

Input + status bar form a fixed “center of mass” panel. The 12-line viewport
never resizes for menus. Menu content appears below the status bar and
disappears on dismiss.

**Three interaction patterns**, all sharing `menu_area`:

| Pattern | Widget | Examples |
|---------|--------|----------|
| 1a. Select | `DropdownState<T>` with type-to-filter, scroll | `/model`, `/provider`, `/` commands, `@file` |
| 1b. Confirm | Compact approval + optional diff preview | Tool approval, file edits |
| 2. Multi-step | Sequential inline prompts with compact trail | `/provider` setup, `/mcp add`, onboarding |

**Key architectural decisions**:
- Per-command state machine enums (`ProviderWizard`, `McpAddWizard`), not a
  generic wizard framework — only 3 commands need multi-step flows (YAGNI)
- Shared `WizardView { trail, active_widget }` for rendering; command-specific
  logic in typed enums with exhaustive `match`
- Power-user escape hatch: positional args skip the wizard entirely
  (`/provider anthropic sk-ant-xxx` → zero prompts)
- Keystore eliminates repeat wizards — most provider switches are instant
- Shared `validate_and_build` between wizard completion and positional parser (DRY)
- No “go back” in v0.2 — Esc to cancel and restart is fine for 2–4 step flows

**Competitive analysis and detailed design**: [#230](https://github.com/lijunzh/koda/issues/230)
**Implementation (slash dropdown)**: [#229](https://github.com/lijunzh/koda/pull/229)

### 15. No `?` Help Overlay — The Dropdown Is Help (v0.1.4)

**Decision**: Removed the `?` keyboard shortcut overlay and `/help` command.
The slash dropdown with descriptions IS the help system.

**Rationale**: Three overlapping discovery mechanisms (`?` overlay, `/help`
modal, `/` auto-dropdown) created redundant complexity and viewport resize
bugs. The auto-dropdown on `/` shows all commands with descriptions — that
is help. Keyboard shortcuts moved to the startup banner header.

**Code removed**: `widgets/help_overlay.rs` (96 lines), `handle_help()`,
`show_help` state, and all associated viewport resize logic.

**Implementation**: [#229](https://github.com/lijunzh/koda/pull/229)

### 16. Adaptive Phase-Gated Agent Loop (v0.1.4)

**Decision**: Koda tracks a six-phase state machine per conversation turn:
Understanding → Planning → Reviewing → Executing → Verifying → Reporting.
Phase transitions are detected structurally from tool-use signals, not by
parsing LLM text output.

**Design reference**: [#216](https://github.com/lijunzh/koda/issues/216)
(original OPAR design), [#242](https://github.com/lijunzh/koda/issues/242)
(implementation plan with Tang Dynasty bureaucracy mapping).

**Key components**:
- `PhaseTracker` (`task_phase.rs`): state machine with `advance(signal)` that
  returns `Option<PhaseTransition>` on phase change. Decision tree uses
  `(current_phase, has_tool_calls, tool_type)` — no LLM output parsing.
- `PhaseInfo`: snapshot of tracker state passed to `check_tool()` for
  phase-aware approval decisions.
- `Role::Phase` messages: phase transitions logged to the DB as structured
  messages. Human-readable summary + JSON metadata. The LLM sees these for
  process self-awareness; the InterventionObserver parses the metadata.
- `InterventionObserver`: per-phase override frequency tracker. Learns from
  auto/override data points at phase gates. Not yet wired into the inference
  loop — data structure and persistence only in v0.1.4.

**Phase-aware approval** (`check_tool()`):
- Understanding/Planning: writes require confirmation even in Auto mode
  (the agent hasn't formed a plan yet)
- Reviewing: writes blocked (forced through the review gate)
- Executing with `plan_approved`: writes auto-approved
- Destructive operations: hardcoded floor regardless of phase

**Escalation and rejection**:
- Executing → Understanding ("escalation"): tool failure suggests scope
  changed (e.g., merge conflict). Explicit, logged transition.
- Reviewing → Planning ("封驳"): LLM self-reflection or human review finds
  the plan unsound.

**Philosophy**: The process adapts to the task, not the other way around.
Simple tasks shortcut (Understanding → Executing). Complex tasks get full
six-phase progression. The human's level of involvement is learned, not
configured. See Design Principles above.

### 17. Folder-Scoped Permissions (v0.1.4)

**Decision**: Writes outside `project_root` always require explicit
confirmation, regardless of approval mode or phase. Bash commands are
linted for path escapes before execution.

**Design reference**: [#218](https://github.com/lijunzh/koda/issues/218)

**Three layers** (defense in depth):
1. `safe_resolve_path()` (existed pre-v0.1.4): blocks path traversal at
   the execution layer for file tools.
2. `is_outside_project()` (v0.1.4): checks path args at the approval layer
   with a clear warning. Hardcoded floor of NeedsConfirmation.
3. `lint_bash_paths()` (v0.1.4): heuristic analysis of bash commands for
   `cd` escapes, absolute paths, and `../` traversals.

**Bash lint decisions**:
- `cd /outside`: flagged
- `cd ~` / bare `cd`: flagged (resolves to $HOME)
- `cd $VAR` / `cd $(cmd)`: ignored (can't resolve statically)
- Chained `cd a && cd b`: first target only
- Symlinks: deferred to v0.1.5 (#280)

**Threat model**: The LLM is semi-trusted (can make mistakes, not adversarial).
The concern is accidental blast radius, not targeted attacks. The lint catches
common accidental escapes; OS-level sandboxing (seccomp/landlock) is a v1.0
concern.

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
