# `/compact` Command Comparison: Koda vs Code Puppy vs Goose vs Claude Code

*Generated 2026-03-02 — comparing compaction/context management across 4 AI code agents*

---

## TL;DR

Koda's `/compact` is **simple and correct** but missing several battle-tested features the others have learned the hard way. The core summarization-via-LLM approach is right, but there are 4-5 improvements worth considering.

---

## Feature Matrix

| Feature | Koda | Code Puppy | Goose | Claude Code |
|---|---|---|---|---|
| **Auto-compact threshold** | 80% | 85% (configurable) | 80% (configurable) | ~80% |
| **Manual command** | `/compact` | `/compact` | `/summarize` + UI button | `/compact` |
| **Strategy options** | Summarization only | Summarization + Truncation | Summarize + Truncate + Clear + Prompt | Summarization |
| **Configurable threshold** | ❌ hardcoded | ✅ `/set` | ✅ env var | ✅ `/config` |
| **Configurable strategy** | ❌ | ✅ `/set` | ✅ env var | ❌ |
| **Protected recent messages** | ❌ all replaced | ✅ token-based tail | ✅ preserves last user msg | ✅ |
| **Tool call safety** | ❌ | ✅ defers if pending | ❌ (implicit) | ✅ |
| **Progressive retry** | ❌ | ❌ | ✅ removes tool responses 0→100% | ❌ |
| **Message visibility** | ❌ deletes originals | ❌ replaces in-memory | ✅ agent/user visible flags | ✅ agent/user visible |
| **Pre-compact hooks** | ❌ | ❌ | ❌ | ✅ plugin hooks |
| **Prompt template** | Hardcoded | Hardcoded | ✅ customizable `.md` | Unknown |
| **Strips images/PDFs** | ❌ | ❌ | ❌ | ✅ |
| **Minimum msg guard** | ✅ (4 msgs) | ❌ | ❌ | ❌ |
| **Token reporting** | ✅ approx | ✅ detailed | ✅ | ✅ |
| **Summary wrapping** | ✅ prefix tag | ✅ bulleted list | ✅ structured sections | ✅ continuation msg |
| **Continuation hint** | ❌ | ❌ | ✅ 3 different texts | ✅ |
| **Persists to DB** | ✅ SQLite | ❌ in-memory only | ✅ session JSON | ✅ |
| **Handles compaction-of-compaction** | ❌ | ✅ hash tracking | ✅ visibility metadata | ✅ compact boundaries |

---

## Deep Dive by Tool

### 🦊 Koda (yours)

**Approach**: Simple and clean. On `/compact` or auto-trigger at 80%, load all messages, format them (truncating each to 2000 chars, total to 20,000 chars), ask the LLM to summarize with bullet points, then **DELETE all messages** from SQLite and insert a single summary as a `user` message.

**Strengths**:
- Dead simple — ~100 lines of logic
- SQLite persistence means compacted state survives restarts
- 4-message minimum guard prevents wasteful compaction
- Silent mode for auto-compact is nice UX

**Weaknesses**:
1. **Destructive**: Deletes ALL messages including recent ones. If you just asked a question and auto-compact fires, the LLM loses your most recent message entirely.
2. **No tool call safety**: If compaction triggers mid-tool-loop, the summary may include incompletes that confuse the LLM on resume.
3. **Hardcoded threshold**: No way to tune the 80% without editing source.
4. **No continuation context**: The LLM gets `[This is a compacted summary...]` but no instruction like "don't mention the summary, just continue naturally."
5. **Summary-as-user-message**: Inserting the summary as a `user` message is semantically wrong — it's not something the user said. (Code Puppy and Goose both handle this more carefully.)
6. **No progressive fallback**: If the summary itself exceeds context, there's no retry strategy.

---

### 🐶 Code Puppy

**Approach**: Two strategies (summarization via a dedicated PydanticAI agent, or simple truncation). Uses a "protected tail" — recent messages within a token budget are preserved verbatim while older messages get summarized. Defers compaction if tool calls are pending.

**Strengths**:
- Dual strategy (summarization/truncation) with user choice
- Protected-tail pattern preserves recent context
- Race condition protection (delayed compaction while tools execute)
- Hash tracking prevents re-compacting already-compacted messages
- Prunes orphaned tool calls before summarizing
- Filters oversized messages before sending to summarizer

**Weaknesses**:
- In-memory only — compaction doesn't persist across restarts
- The dedicated summarization agent adds latency + complexity
- Thread pool management for sync/async bridge is fragile

---

### 🪿 Goose

**Approach**: The most sophisticated. Uses a customizable Markdown prompt template with structured sections (User Intent, Technical Concepts, Files+Code, Errors+Fixes, Pending Tasks, Current Work, Next Step). Has progressive retry — if the summary itself exceeds context, it removes 10% → 20% → 50% → 100% of tool responses from the middle out.

**Strengths**:
- **Best summarization prompt** — structured sections preserve the most useful context
- Customizable prompt template (users can edit `compaction.md`)
- Progressive retry on context exceeded is brilliant
- Message visibility metadata means original messages stay visible to the user (scroll-back) but hidden from the agent
- 3 different continuation texts (conversation, tool-loop, manual) help the LLM resume correctly
- Preserves the most recent user message for auto-compact
- Tool pair summarization (background, incremental — currently disabled but the architecture is there)

**Weaknesses**:
- Most complex implementation (~350 lines for core + tests)
- Tool pair summarization disabled due to stability issues
- No deferred compaction for pending tool calls (relies on the agent loop timing)

---

### 🤖 Claude Code

**Approach**: Closed-source core, but from the CHANGELOG we can infer: summarization-based compaction with agent/user visibility flags, compact boundaries (don't re-compact already compacted regions), pre-compact plugin hooks, and image/PDF stripping.

**Strengths**:
- **PreCompact hooks** let plugins inject critical info before compaction
- Strips binary content (images, PDFs) before summarizing
- Preserves plan mode and session names through compaction
- Compact boundaries prevent recursive compaction issues
- Memory cleanup after compaction (clears internal caches)

**Weaknesses**:
- Closed source, can't study implementation details
- Has had MANY compaction bugs (at least 15 changelog entries fixing compaction issues)
- Auto-compact used to trigger too early, run twice, fail on PDFs, lose plan mode, etc.

---

## Recommendations for Koda

### 🔴 Must Fix

1. **Preserve recent messages** — Don't delete everything. Keep at least the last user message and the last assistant response. Goose's approach of preserving the most recent user message is minimal but effective.

2. **Add continuation instruction** — After the summary, inject an assistant message like: *"Your context was compacted. Continue naturally without mentioning the summary."* All three competitors do this.

3. **Don't insert summary as `user` role** — The summary should either be a `system` message or an `assistant` message. Having the user "say" the summary confuses the model's role understanding.

### 🟡 Should Do

4. **Make threshold configurable** — Add it to `KodaConfig`. One line: `auto_compact_threshold: usize` with a `/set` command. All competitors have this.

5. **Defer compaction during tool execution** — Check if the last message is a tool call without a response. If so, defer compaction to the next turn. Code Puppy's approach is clean.

6. **Improve the summarization prompt** — Koda's prompt is generic ("Summarize this conversation concisely"). Goose's structured sections (User Intent, Files+Code, Pending Tasks, Next Step) produce much better summaries for code work.

### 🟢 Nice to Have

7. **Progressive retry** — If the summary call itself hits context limits, strip tool call details and retry. Goose's middle-out removal is clever.

8. **Customizable prompt** — Let users override the summarization prompt via a file (like Goose's `compaction.md`).

9. **Message visibility instead of deletion** — Instead of deleting rows, add a `compacted` boolean column. This preserves history for debugging and lets users scroll back.

---

## Summary

Koda's design philosophy is right: **LLM-based summarization at a threshold**. That's what everyone converged on. But the implementation is v1-simple where the competitors have battle-hardened theirs through dozens of bug fixes. The biggest gaps are:

1. Preserving recent messages (all three competitors do this)
2. Continuation instructions (all three competitors do this)
3. Correct role assignment for the summary message
4. Configurable threshold

Fix those 4 and you're on par. The rest is polish.
