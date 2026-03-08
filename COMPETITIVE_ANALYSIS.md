# Koda Competitive Analysis

*Generated 2026-03-07. Compared against Claude Code, Cursor, Aider, Goose, Zed Agent, Continue, Copilot.*

## Current State

- **25K lines** across 4 crates (core: ~15K, cli: ~8K, ast: ~500, email: ~500)
- **563 tests** (all passing)
- **22 built-in tools**, 6 agents, 2 skills, 2 MCP servers
- **14 LLM providers** (most in the space)
- **3-tier model adaptation** (unique — no competitor does this)

---

## (1) Architecture: Do We Need to Improve?

### What's Right

| Decision | Validation |
|----------|------------|
| Engine-as-library (zero IO) | Same pattern as Zed. Best-in-class. |
| Event protocol | Clean, serializable, transport-agnostic. Ready for GUI/web clients. |
| ACP server | Zed + Goose converged on this independently. |
| MCP extensibility | Industry standard. Claude Code, Cursor, Zed all use it. |
| Match-based tool dispatch | Compile-time safety. Right at current scale (~22 tools). |
| Sub-agent model routing | Unique cost lever. Nobody else does cheap-model scouts. |

**Verdict: The architecture is sound.** No structural rewrites needed.

### What Needs Evolution (Not Rewrites)

#### A. No Persistence Trait

DESIGN.md already calls this out. `db.rs` is a concrete SQLite implementation.
When we add vector embeddings (see §3), we'll need a trait boundary.

**Risk**: Low right now, but becomes blocking when indexing lands.

**Recommendation**: Define `trait Persistence` with the current SQLite as the
first impl. Do this *before* adding a vector store, not during.

#### B. No Git Module in Core

Git operations go through the `Bash` tool (`git diff`, `git commit`, etc.).
Every competitor with checkpointing has a proper git abstraction:

| Competitor | Git Integration |
|-----------|----------------|
| Claude Code | Auto-checkpoint before changes, `/undo` rolls back |
| Aider | Git-aware context, auto-commit, diff-based edits |
| Cursor | Git lens, inline blame, branch-aware context |
| Koda | Bash("git ..."), in-memory UndoStack |

Our `UndoStack` (file snapshots) is in-memory and doesn't survive crashes.
A git-based checkpoint would be more durable and familiar to developers.

**Recommendation**: Add `koda-core/src/git.rs` with:
- `checkpoint()` — stash or commit before a turn
- `rollback()` — revert to last checkpoint
- `diff_context()` — staged/unstaged diffs for auto-injection into context
- `is_repo()` / `current_branch()` — basic repo awareness

#### C. No Indexing / Embedding Layer

This is the **single biggest architectural gap** vs Cursor and Continue.

| Approach | Who Uses It | Koda |
|----------|------------|------|
| Embeddings + vector search | Cursor, Continue | ❌ |
| Keyword search (grep/glob) | Aider, Claude Code | ✅ |
| Tree-sitter AST | Zed, Koda (via MCP) | ✅ |
| LSP integration | Cursor, Zed | ❌ |

For large codebases (>100K lines), grep is insufficient — you need semantic
search to find *conceptually* related code, not just keyword matches.

**Recommendation**: This is a v0.2.x feature. Options:
1. Embed via local model (e.g., `nomic-embed-text` on Ollama) — zero API cost
2. Use an MCP server for embeddings (keeps core lean)
3. SQLite FTS5 for full-text search as a middle ground (no ML needed)

FTS5 is the pragmatic first step — it's already in our SQLite dep.

---

## (2) Core Library: Is Anything Missing or Over-Removed?

### Module Audit

| Module | Status | Notes |
|--------|--------|-------|
| `agent.rs` | ✅ Solid | Clean KodaAgent with Arc sharing |
| `approval.rs` | ✅ Solid | 3 modes, async flow, bash safety |
| `bash_safety.rs` | ✅ Solid | Command classification |
| `compact.rs` | ✅ Solid | Auto + manual compaction |
| `config.rs` | ✅ Solid | Agent JSON + CLI + env layering |
| `context.rs` | ✅ Solid | Token tracking |
| `db.rs` | ✅ Solid | WAL mode, parameterized queries |
| `engine/` | ✅ Solid | Clean event protocol |
| `inference.rs` | ✅ Solid | 3-tier dispatch, sub-agent cache |
| `inference_helpers.rs` | ✅ Solid | Token estimation, overflow detection |
| `intent.rs` | ⚠️ Limited | Rule-based only, no learning |
| `keystore.rs` | ✅ Solid | Secure storage (0600 perms) |
| `loop_guard.rs` | ✅ Solid | Pattern detection + hard cap |
| `mcp/` | ✅ Solid | Auto-provision, capability registry |
| `memory.rs` | ✅ Solid | Project + global tiers |
| `model_context.rs` | ✅ Solid | Fallback lookup table |
| `model_tier.rs` | ✅ Solid | Unique in the market |
| `output_caps.rs` | ✅ Solid | Context-scaled limits |
| `preview.rs` | ✅ Solid | Diff previews before confirmation |
| `progress.rs` | ✅ Solid | Task tracking |
| `prompt.rs` | ✅ Solid | Tier-aware prompt construction |
| `providers/` | ✅ Solid | 4 impls covering 14 providers |
| `runtime_env.rs` | ✅ Solid | Thread-safe env access |
| `session.rs` | ✅ Solid | Per-conversation state |
| `skills.rs` | ⚠️ Thin | Only 2 skills, no community ecosystem |
| `sub_agent_cache.rs` | ✅ New | Generation-based invalidation |
| `task_phase.rs` | ⚠️ Passive | Tracks phase but doesn't adapt behavior |
| `tier_observer.rs` | ✅ Solid | Runtime promotion/demotion |
| `tool_dispatch.rs` | ✅ Solid | Parallel + split-batch + cache |
| `tools/` | ✅ Solid | 22 tools, MCP fallback |
| `undo.rs` | ⚠️ Fragile | In-memory only, lost on crash |
| `version.rs` | ✅ Solid | Background version check |

### What's Missing From Core

#### 1. `git.rs` — Git Abstraction
See §1B above. Every serious competitor has this.

#### 2. `index.rs` — Project Indexing
See §1C above. Even a basic file-list cache with FTS5 would help.

#### 3. `rules.rs` — Project Rules (`.koda.md`)

Cursor has `.cursorrules`. Claude Code has `CLAUDE.md`. Aider has `.aider.conf.yml`.
Koda has `MEMORY.md` which is close, but it's manually written and not
auto-loaded into the system prompt with the same ceremony.

**Koda should auto-detect and load `.koda.md` (or `KODA.md`) from project root
into the system prompt.** This is a 30-line feature with huge developer UX impact.

#### 4. `structured_output.rs` — JSON Mode

For CI/CD and automation, the engine should support requesting structured
JSON output from the LLM. Claude Code's `--output-format json` is heavily
used in GitHub Actions.

#### 5. No `diff.rs` — Multi-File Transaction Preview

Koda previews single-file diffs. Claude Code and Cursor show all pending
changes across files before confirmation. When the LLM edits 5 files,
you want to see the full changeset, not 5 separate approval prompts.

### What Was Correctly Kept

Nothing critical was over-removed. The v0.1.2 cleanup (deleted `app.rs`,
`display.rs`, `markdown.rs`, `confirm.rs` — 2,454 lines of legacy code)
was the right call. The current module set is lean and cohesive.

---

## (3) Missing Features

### Tier 1: High Impact, Competitors All Have

| # | Feature | Who Has It | Effort | Impact |
|---|---------|-----------|--------|--------|
| 1 | **Project rules file** (`.koda.md`) | Cursor, Claude Code, Aider | S | 🔴 High |
| 2 | **Git checkpointing** (auto-commit before changes) | Claude Code, Aider | M | 🔴 High |
| 3 | **Git-aware context** (auto-inject relevant diffs) | Aider, Claude Code | M | 🔴 High |
| 4 | **Multi-file change preview** (batch approval) | Claude Code, Cursor | M | 🔴 High |
| 5 | **Structured JSON output** (`--output-format json`) | Claude Code | S | 🟡 Med-High |
| 6 | **Semantic search** (FTS5 or embeddings) | Cursor, Continue | L | 🔴 High |

### Tier 2: Differentiators We Should Build

| # | Feature | Notes | Effort |
|---|---------|-------|--------|
| 7 | **Cost budgets** — max spend per session/turn | Nobody does this well. We track cost but don't cap it. | S |
| 8 | **Conversation branching** — fork to try alternatives | Unique differentiator. Git-like branching for conversations. | M |
| 9 | **Batch API support** (#196) | Anthropic Batch API for async review at 50% cost. Already filed. | M |
| 10 | **Background file watching** — detect external edits | Invalidate read cache, warn about conflicts. | S |
| 11 | **Session export** — export conversation as markdown | For documentation, sharing, post-mortems. | S |

### Tier 3: Nice to Have

| # | Feature | Notes | Effort |
|---|---------|-------|--------|
| 12 | **Browser tool** — headless Chrome for web debugging | Claude Code has this. MCP server candidate. | M |
| 13 | **Clipboard integration** — paste images/code | Terminal limitation, but iTerm2/kitty support it. | S |
| 14 | **Auto-update** — self-update mechanism | `koda update` or background check + prompt. | S |
| 15 | **Themes** — customizable colors | Low priority but users love it. | S |
| 16 | **Telemetry** — opt-in usage analytics | For prioritizing features. Privacy-first design. | M |

---

## Competitive Matrix

| Capability | Koda | Claude Code | Cursor | Aider | Goose | Zed |
|-----------|------|------------|--------|-------|-------|-----|
| **Core** | | | | | | |
| Open source | ✅ | ❌ | ❌ | ✅ | ✅ | ✅ |
| Single binary | ✅ | ❌ (npm) | ❌ (Electron) | ❌ (Python) | ✅ | ✅ |
| Multi-provider | ✅ (14) | ❌ (1) | ✅ (~5) | ✅ (~10) | ✅ (~5) | ✅ (~3) |
| Local model support | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ |
| MCP support | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ |
| **Intelligence** | | | | | | |
| Model-adaptive tiers | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Lazy tool loading | ✅ | ❌ | N/A | ❌ | ❌ | ❌ |
| Sub-agent orchestration | ✅ | ❌ | ❌ | ❌ | ✅ | ✅ |
| Sub-agent model routing | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Result caching | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Intent classification | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Context** | | | | | | |
| Semantic search | ❌ | ❌ | ✅ | ❌ | ❌ | ✅ |
| Git-aware context | ❌ | ✅ | ✅ | ✅ | ❌ | ✅ |
| Project rules file | ❌ | ✅ | ✅ | ✅ | ❌ | ❌ |
| Persistent memory | ✅ | ✅ | ❌ | ❌ | ❌ | ❌ |
| Auto-compaction | ✅ | ✅ | N/A | ✅ | ✅ | ❌ |
| Context from API | ✅ | N/A | N/A | ❌ | ❌ | ❌ |
| **Editing** | | | | | | |
| Diff preview | ✅ | ✅ | ✅ | ✅ | ❌ | ✅ |
| Multi-file preview | ❌ | ✅ | ✅ | ❌ | ❌ | ✅ |
| Git checkpointing | ❌ | ✅ | ❌ | ✅ | ❌ | ❌ |
| Undo | ✅ (memory) | ✅ (git) | ✅ | ✅ (git) | ❌ | ✅ |
| **Operations** | | | | | | |
| Cost tracking | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ |
| Cost budgets | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Headless/CI mode | ✅ | ✅ | ❌ | ✅ | ✅ | ❌ |
| JSON output | ❌ | ✅ | N/A | ❌ | ✅ | N/A |
| Rate limit retry | ✅ | ✅ | N/A | ✅ | ❌ | ❌ |
| ACP/editor integration | ✅ | ❌ | N/A | ❌ | ✅ | N/A |

---

## Recommended Priority

### Next sprint (v0.1.4 or v0.1.5)
1. **`.koda.md` project rules** — 30 lines, massive UX win, table stakes
2. **Git checkpointing** — `git stash create` before turns, `/undo` uses `git stash pop`
3. **Structured JSON output** — `--output-format json` for CI/CD
4. **Cost budgets** — `--max-cost 1.00` to cap session spend

### v0.2.x
5. **Git-aware context** — auto-inject staged diffs + recent commits
6. **Multi-file change preview** — batch approval for multi-file edits
7. **FTS5 full-text search** — index project files in SQLite for semantic-ish search
8. **Session export** — `koda export --format markdown`

### v0.3.x
9. **Embedding-based search** — local model or MCP server
10. **Conversation branching** — unique differentiator
11. **Browser tool** — headless Chrome MCP server
12. **Persistence trait** — abstract DB backend for future stores

---

## Summary

**Architecture**: Solid. No rewrites needed. The engine-as-library + event protocol +
MCP extensibility pattern is validated by Zed and Goose independently arriving at the
same design. The model-adaptive tier system and sub-agent cost routing are unique
differentiators that no competitor has.

**Core library**: Complete for current scope. Missing pieces are additive (git, indexing,
rules), not replacements. Nothing was over-removed in recent refactors.

**Features**: The biggest gaps are table-stakes features that competitors all have:
project rules file, git checkpointing, and structured output. These are all small-to-medium
effort. The larger gap (semantic search) is a v0.2.x investment. Koda's unique strengths
(multi-provider, model adaptation, sub-agent routing, cost tracking) are genuine
differentiators that should be amplified, not abandoned.

**TL;DR**: Ship `.koda.md` + git checkpoint + JSON output. Then build toward semantic search.
The architecture supports all of it without structural changes.
