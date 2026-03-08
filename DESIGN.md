# Koda Design Decisions

This document captures architectural decisions and their rationale.
For the TUI architecture (v0.1.2), see [#70](https://github.com/lijunzh/koda/issues/70).
For workspace structure and developer docs, see [CLAUDE.md](CLAUDE.md).

## Vision

Koda is a personal AI assistant. Coding is the starting point, but the platform
will expand to support email, messaging, calendar, reminders, documentation,
and knowledge management — all powered by the same engine.

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

## References

- [ACP (Agent Client Protocol)](https://www.npmjs.com/package/@agentclientprotocol/sdk)
- [Zed Agent Architecture](https://github.com/zed-industries/zed/tree/main/crates/agent)
- [Goose ACP Server](https://github.com/block/goose/tree/main/crates/goose-acp)
- [xi-editor Frontend Protocol](https://xi-editor.io/docs/frontend-protocol.html)
- [Neovim API](https://neovim.io/doc/user/api.html)
