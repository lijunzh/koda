# Koda Design Decisions

This document captures architectural decisions and their rationale.
For the TUI architecture (v0.1.2), see [#70](https://github.com/lijunzh/koda/issues/70).
For workspace structure and developer docs, see [CLAUDE.md](CLAUDE.md).

## Vision

Koda is a personal AI assistant. Coding is the starting point, but the platform
will expand to support email, messaging, calendar, reminders, documentation,
and knowledge management — all powered by the same engine.

## Design Principles

Principles are truths we enforce on the product. They may not be correct for
everyone, but we follow them anyway. Design decisions (§1–§19 below) are
examples that follow — or violated — these principles.

### 1. Software for One

AI changes how software is built. We no longer need configurable software
that caters to a broad audience through options and flags. Instead, we build
hyper-targeted software for a single user — the author — whose needs can
be changed with a few prompts and a recompile.

This is not a limitation. It is a superpower:

- **Customization over configuration.** If a decision can be made at compile
  time, it must be. Rust excels at compile-time safety; runtime configuration
  defeats it. Flags that select an execution scenario are fine (`-p` for
  headless, `server --stdio` for ACP) — flags that alter behavior within a
  scenario are not (`--autonomy`, `--model-tier`). If something needs to
  change, change the code
- **Build only what we need.** Don't anticipate what users might want.
  There is one user. Code that isn't written has zero bugs. Features that
  were built but aren't used should be deleted — git preserves history
- **Delete aggressively.** Carrying dead code forward degrades every future
  decision because it obscures what the system actually does. No
  "extensibility for later" — trait abstractions and plugin systems have a
  cost even when idle

### 2. Clear Boundaries

Every component has a sharp boundary — what it does, what it doesn't,
and where responsibility transfers to the next component.

- **Engine** (`koda-core`): communicate with the LLM, curate context,
  execute tools, manage safety. Zero terminal deps. Zero UI opinions
- **UI** (`koda-cli`): deliver the best UX. Render events, capture input,
  present approvals. Zero inference decisions
- **Model**: plan, reason, decide which tools to call. The engine does NOT
  reimplement planning, verification, or decision-making in application code
- **Provider**: koda targets a single model family. Don't adapt to different
  model capabilities at runtime. If a model can't meet the contract, it
  fails — the engine doesn't bend to accommodate it

These boundaries are load-bearing. Breaking them causes the exact class of
bugs that motivated removing the phase system (§16), the model tier system
(§8), and the intervention observer.

### 3. Make It Work, Make It Right, Make It Fast

Don't optimize prematurely. Ship working code first, refactor to clean
design second, optimize for performance only when measured. This applies
to architecture too — don't design for scale that doesn't exist yet.

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

### 8. Model Capability Probe (v0.1.4)

**Decision**: Replace the three-tier model gradient (Strong/Standard/Lite) with
a binary startup probe. Can this model handle koda's contract? Yes → full trust.
No → fail loudly.

**What was deleted** (v0.1.4, [#332](https://github.com/lijunzh/koda/pull/332)):
- `model_tier.rs` — Strong/Standard/Lite enum
- `tier_observer.rs` — dynamic promotion/demotion based on tool-call quality
- Tier-specific prompt personas (`build_strong_persona`, `build_lite_persona`)
- `--model-tier` CLI flag
- `get_definitions_tiered()` (Strong-tier tool filtering)

**What replaced it**: `model_probe.rs` — one inference call at session start
that asks the model to emit structured JSON with specific keys. Binary pass/fail.
Cached per model name in `~/.config/koda/model_probes.json`. Skippable with
`--skip-probe`.

**Rationale**: The three-tier system was configuration masquerading as
adaptation. Tier-specific prompts were hedging against model uncertainty by
coddling weaker models with verbose instructions. In practice, models either
handle koda's structured tool-use contract or they don't — there's no useful
middle ground. A model that can't emit valid JSON tool calls won't improve
with a more verbose prompt; it'll just fail in more verbose ways.

**Philosophy**: The probe replaces hedging with a hard gate at the only moment
you can't check at compile time — model identity is inherently a runtime fact.

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

### 10. ~~Lazy Tool Loading with DiscoverTools (v0.1.3)~~ Removed (v0.1.4)

**Removed in** [#332](https://github.com/lijunzh/koda/pull/332) as part of
the ModelTier deletion. All models now receive all tool schemas. The
`DiscoverTools` tool and `get_definitions_tiered()` filtering were deleted.
Dead code (`tools/discover.rs`, registration, and dispatch) fully cleaned up
in [#402](https://github.com/lijunzh/koda/issues/402).

**Original rationale**: 20+ tool schemas cost ~2000 tokens/turn, so Strong-tier
models got only 9 core tools upfront. In practice, the lazy loading added
complexity without proportional benefit — most models handle the full tool
set fine, and the binary probe gate ensures they can.

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

**Rationale**: *The conversation is the primary surface. Interactions happen
within it, not on top of it.* This is the common thread across Claude Code
and Codex. Goose's stepped wizards and Code Puppy's fullscreen forms violate
this — users find them tedious and disorienting.

**Key choices**:
- Per-command state machine enums, not a generic wizard framework (YAGNI)
- Power-user escape hatch: positional args skip wizards entirely
- No "go back" — Esc to cancel and restart is fine for 2–4 step flows

For the viewport layout diagram and interaction patterns, see
[docs/user-guide.md](docs/user-guide.md#slash-commands).

**Competitive analysis**: [#230](https://github.com/lijunzh/koda/issues/230)
**Implementation**: [#229](https://github.com/lijunzh/koda/pull/229)

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

### 16. ~~Adaptive Phase-Gated Agent Loop~~ (v0.1.4 — RETIRED in #355)

**Decision**: RETIRED. The six-phase state machine (Understanding → Planning →
Reviewing → Executing → Verifying → Reporting) was fully implemented and then
stripped in [#355](https://github.com/lijunzh/koda/pull/355) (-4,308 lines).

**Why it was removed**: Formal plan submission cost ~500 tokens/turn in schema
overhead. SelfReview re-sent the entire context for the same model to review
its own output. Strong models plan naturally; weak models couldn’t follow the
protocol. The state machine became the primary source of bugs (7 PRs to fix
one bug, #342). See [#216 post-mortem](https://github.com/lijunzh/koda/issues/216#issuecomment-4035670832).

**What survived**: Per-tool safety gates (`ToolEffect` → `check_tool()`),
folder-scoped permissions (§17), and the principle that the LLM’s extended
thinking IS the planning.

**Archive**: Tag `v0.1.4-phase-system` preserves the full implementation.

### 17. Folder-Scoped Permissions (v0.1.4)

**Decision**: Writes outside `project_root` always require explicit
confirmation, regardless of approval mode. Bash commands are
linted for path escapes before execution.

**Rationale**: Defense in depth with three layers — path resolution at
execution, path checks at approval, and heuristic bash linting. The LLM
is semi-trusted (can make mistakes, not adversarial). The concern is
accidental blast radius, not targeted attacks.

**Design reference**: [#218](https://github.com/lijunzh/koda/issues/218).
For operational details, see [docs/user-guide.md](docs/user-guide.md#security-model).

### 18. Security Model (v0.1.4)

**Decision**: Per-tool safety classification with two approval modes and
hardcoded floors that override mode settings for high-risk operations.

**Rationale**: The LLM is semi-trusted — capable of mistakes, not adversarial.
Every tool call is classified into one of four effects (ReadOnly, LocalMutation,
Destructive, RemoteAction). Approval modes (Auto/Confirm) determine which
effects need confirmation. Hardcoded floors ensure destructive operations and
outside-project writes always require confirmation regardless of mode.

For approval mode tables, tool effect matrix, and operational details, see
[docs/user-guide.md](docs/user-guide.md#security-model).

**Key design choices**:
- Sub-agent delegation via `DelegationScope` (mode clamping, filesystem grants,
  tool allowlists) — enforcement is a hard gate, not a log
- MCP tool classification from schema annotations (`readOnlyHint`, `destructiveHint`)
  with `.mcp.json` overrides taking precedence
- No kernel-level sandboxing yet — seccomp/landlock is a v1.0 concern

**Accepted risks**:
1. No kernel-level sandboxing — in-process only
2. Shell command parsing is heuristic — complex pipelines can bypass
3. MCP `readOnly` is trust-based — malicious servers could lie
4. Auto mode sub-agents with `FullProject` scope get full write access
5. Outside-project writes in Confirm mode show confirm prompt instead of clean block

### 19. ~~Review Depth as Isolation Boundaries~~ (v0.1.4 — RETIRED in #355)

**Decision**: RETIRED along with the phase system in [#355](https://github.com/lijunzh/koda/pull/355).
The concept of asymmetric model collaboration (weak model asks questions,
strong model answers) remains valuable and may return as a standalone
`/review` command ([#256](https://github.com/lijua/issues/256)).

**Archive**: Tag `v0.1.4-phase-system` preserves the full implementation.

## Principles Audit (v0.1.6)

How existing design decisions align with the core principles. Decisions that
violate the principles are tracked as issues for future cleanup.

### Aligned

| Decision | Principle | Why it aligns |
|----------|-----------|---------------|
| §1 Engine as library | Clear Boundaries | Engine has zero terminal deps, communicates only via events |
| §5 Monolithic db.rs | Software for One | Resisted premature abstraction; split by domain divergence, not line count |
| §7 Match dispatch | Software for One | Compile-time exhaustive matching > runtime `HashMap<String, Box<dyn Tool>>` |
| §8 Binary probe > model tiers | Clear Boundaries | Removed 3-tier runtime adaptation; models meet the contract or fail |
| §13 CLAUDE.md not .koda.md | Software for One | One file for all tools; rejected redundant config surface |
| §14 Inline UI, never fullscreen | Software for One | No generic wizard framework (YAGNI); per-command state machines |
| §15 Dropdown is help | Software for One | Removed 3 overlapping discovery mechanisms → 1 |
| §16 Phase system retired | Clear Boundaries | Removed 4,308 lines of planning that reimplemented what the model does |
| §17 Folder-scoped permissions | Software for One | Hardcoded safety floors, not configurable trust levels |
| §18 Security model | Software for One | ToolEffect classification is compile-time; approval modes are the only runtime knob |

### Violations (tracked for cleanup)

| Area | Violation | Principle | Severity | Issue |
|------|-----------|-----------|----------|-------|
| `model_context.rs` | 250-line lookup table for 50+ models across 14 providers. 95% unused if targeting Claude | Software for One | Medium | [#401] |
| `output_caps.rs` | Tool output limits scale 1–4× based on context window at runtime | Software for One | Medium | [#401] |
| `query_and_apply_capabilities()` | 6 call sites querying provider APIs to override hardcoded context table | Software for One | Medium | [#401] |
| `model_probe.rs` | Runtime binary gate hedging for weak models that can't follow the contract | Clear Boundaries | Low | [#401] |
| ~~`DiscoverTools`~~ | ~~§10 says removed, but `tools/discover.rs` still exists~~ — **Resolved** in [#402] | Software for One | — | [#402] |
| `DelegationScope` | 140 lines of sub-agent permission scoping; unused if sole user doesn't delegate | Software for One | Medium | [#403] |
| `CreateAgent` tool | LLM-invoked agent file creation; manual JSON is sufficient | Software for One | Low | [#403] |
| `Persistence` trait | Trait abstraction with single SQLite backend; no second backend exists | Software for One | Low | — |
| `thinking_budget` / `reasoning_effort` | Provider-specific optional fields scattered across config; inert if Claude-only | Software for One | Low | [#401] |

**Note**: The `Persistence` trait is retained — its cost is minimal (~50 lines)
and trait-based testing (mock DB) justifies its existence independently of a
second backend.

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
