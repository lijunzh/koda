# Future Feature Requests & Roadmap

Tracking features deferred from v0.1.0, organized by priority.
Based on competitive analysis against Claude Code and Code Puppy.

**Koda's core strength:** Single compiled Rust binary, multi-provider LLM
support, zero runtime dependencies. Neither Claude Code (Node.js) nor
Code Puppy (Python) can match that deployment story.

---

## Quick Wins (v0.1.x) — ALL COMPLETE ✅

All quick wins were implemented before the v0.1.0 release:

### 1.1. `/cost` Command — Token Usage Tracking ✅

**Status:** Implemented in v0.1.0.

---

### 1.2. Auto-Memory — MemoryRead/MemoryWrite + `/memory` ✅

**Status:** Implemented in v0.1.0. Reads `MEMORY.md`, `CLAUDE.md`,
`AGENTS.md` (first wins) + global `~/.config/koda/memory.md`.

---

### 1.3. Session Management — `/sessions` Command ✅

**Status:** Implemented in v0.1.0.

---

### 1.4. Clipboard Integration ✅

**Status:** Implemented in v0.1.0. `/copy` with code block picker, `/paste`.

---

### 1.5. Onboarding Wizard ✅

**Status:** Implemented in v0.1.0.

---

### 1.6. Version Checker ✅

**Status:** Implemented in v0.1.0. Non-blocking crates.io check on startup.

---

## Medium Features (v0.2.0)

Moderate effort, significant capability improvements.

### 2.1. `/compact` — Context Window Compression ✅

**Status:** Implemented. Summarizes conversation history via LLM and replaces
all messages with a single compact summary. Guards against compacting
too-short conversations (<4 messages).

**Effort:** Medium.

**What:** Summarize the current conversation to reclaim context window
space. Useful in long sessions where early messages are no longer
relevant but consume tokens.

**Approach:** Send the conversation to the LLM with a "summarize this
conversation concisely" prompt, replace history with the summary.

**Reference:** Claude Code has `/compact`.

---

### 2.2. Image / Screenshot Analysis ✅

**Status:** Implemented. `@image.png` references detect image files by
extension (png, jpg, jpeg, gif, webp, bmp), base64-encode them, and send
them to the LLM using multi-modal content formats. Works with OpenAI
(image_url data URIs) and Anthropic (image content blocks).

**Effort:** Medium.

**What:** Accept image files as input (via `@image.png` or a dedicated
tool) and send them to multi-modal LLMs for analysis. Useful for
UI debugging, diagram understanding, and error screenshots.

**Considerations:**
- Requires base64 encoding and multi-modal message format
- Only works with providers that support vision (OpenAI, Anthropic, Gemini)
- Could also add screenshot capture via `crossterm` or system commands

**Reference:** Both Claude Code and Code Puppy support image analysis.

---

### 2.3. `/diff` — Uncommitted Changes Review ✅

**Status:** Implemented. `/diff` shows stat summary, `/diff review` sends
full diff for LLM code review, `/diff commit` generates conventional
commit messages.

**Effort:** Medium.

**What:** Show a summary of uncommitted git changes and optionally ask
the LLM to review them, suggest improvements, or write commit messages.

**Reference:** Both Claude Code and Code Puppy have diff review.

---

### 2.4. Notebook Support (Jupyter)

**Effort:** Medium.

**What:** Read and edit `.ipynb` Jupyter notebook files. Parse the JSON
structure to show cell contents, and allow editing individual cells.

**Reference:** Claude Code has `NotebookRead` and `NotebookEdit`.

---

### 2.5. Headless / Non-Interactive Mode

**Effort:** Medium.

**What:** Run Koda as a one-shot CLI tool:
```bash
koda "fix the login bug in src/auth.rs" --headless
```
Execute the task and exit without entering the REPL. Useful for CI/CD
pipelines, scripts, and editor integrations.

**Reference:** Claude Code supports `claude -p "prompt"` mode.

---

### 2.6. Ask User Question — Interactive Multi-Choice TUI

**Effort:** Medium.

**What:** A tool the LLM can call to ask the user a structured question
with multiple-choice options, rendered as an arrow-key selector.

**Reference:** Code Puppy has `ask_user_question` with a full TUI.

---

### 2.7. Model Marketplace Integration

**Effort:** Medium.

**What:** Browse and add models from an API catalog (e.g., models.dev)
via an interactive `/add_model` command.

**Reference:** Code Puppy integrates with models.dev (65+ providers).

---

### 2.8. Prompt Caching (Anthropic)

**Effort:** Medium.

**What:** Use Anthropic's prompt caching API to cache the system prompt
and reduce costs/latency for repeated interactions.

**Reference:** Both Claude Code and Code Puppy support prompt caching.

---

## Large Features (v0.3.0+)

Significant architectural work, differentiating capabilities.

### 3.1. Ratatui-based TUI

**Effort:** Large.

**Motivation:** Several features need a proper TUI framework:
- Transcript fold/unfold (replace in-place, not append)
- Scrollable tool output panels
- Split-pane views (code + conversation)
- Progress bars for long-running tool calls
- Inline diff rendering for file edits
- Resizable/responsive layouts

**Candidate:** [`ratatui`](https://github.com/ratatui/ratatui) —
most actively maintained Rust TUI framework.

**Considerations:**
- `ratatui` takes over the terminal (alternate screen), conflicting
  with the current streaming-print approach
- May need hybrid mode: normal streaming during inference, ratatui
  for interactive panels
- `crossterm` (already a dependency) is a supported ratatui backend

**Subsumes:** Transcript fold/unfold (removed in v0.1.0).

---

### 3.2. MCP Protocol (Model Context Protocol)

**Effort:** Large.

**What:** Support the [Model Context Protocol](https://modelcontextprotocol.io/)
for extensible tool servers. MCP allows third-party tools to be exposed
to the LLM via a standardized JSON-RPC protocol.

**Why it matters:** MCP is becoming the industry standard for AI tool
extensibility. Both Claude Code and Code Puppy have full MCP support.

**Approach:**
- Implement MCP client (connect to external MCP servers)
- Auto-discover tools from connected servers
- Merge MCP tools into the existing tool registry

---

### 3.3. Plugin / Hook System

**Effort:** Large.

**What:** Allow users to extend Koda without forking. Hooks at key
lifecycle points:
- `pre_tool_call` / `post_tool_call`
- `on_edit_file` / `on_delete_file`
- `on_shell_command`
- `on_startup` / `on_shutdown`
- `register_tools` / `register_commands`

**Approach options:**
- JSON/TOML config pointing to shell scripts (lightweight)
- WASM plugin modules (sandboxed, portable)
- Lua/Rhai scripting (embedded, fast)

**Reference:** Claude Code has hooks. Code Puppy has a full callback
system with 30+ lifecycle hooks.

---

### 3.4. IDE Integration (VS Code Extension)

**Effort:** Large.

**What:** VS Code extension that communicates with a running Koda
instance via IPC/WebSocket. Show Koda in a panel, share editor context.

**Reference:** Claude Code has deep VS Code integration.

---

### 3.5. Agent Teams — Parallel Multi-Agent Execution

**Effort:** Large.

**What:** Run multiple agents in parallel on different tasks, optionally
with git worktree isolation so they don't conflict.

**Current state:** Koda supports sequential sub-agent delegation via
`InvokeAgent`. Teams would add parallel execution.

**Reference:** Claude Code has agent teams with worktree isolation.
Code Puppy has a "pack" system (bloodhound, husky, retriever, etc.).

---

### 3.6. Browser Automation

**Effort:** Large.

**What:** Playwright-based browser control for web testing, scraping,
and UI interaction. 30+ tools for navigation, clicks, form filling,
screenshots, and workflow recording.

**Reference:** Code Puppy has full browser automation.

---

### 3.7. Specialized Reviewer Agents

**Effort:** Medium per agent.

**What:** Pre-built agent configs for language-specific code review:
- Python reviewer (PEP 8, type hints, async patterns)
- Rust reviewer (ownership, lifetimes, unsafe audit)
- JavaScript/TypeScript reviewer (ESLint rules, React patterns)
- Security auditor (OWASP, dependency vulnerabilities)
- QA expert (test coverage, edge cases)

**Reference:** Code Puppy has 10+ specialized reviewer agents.

---

### 3.8. Skills Marketplace

**Effort:** Large.

**What:** Downloadable skill packs that inject specialized prompts and
tools for specific domains (e.g., "AWS deployment", "React development",
"database migration").

**Reference:** Code Puppy has a skills system with remote catalog.

---

## Competitive Summary

| Capability | Koda v0.1.0 | Claude Code | Code Puppy |
|------------|:-----------:|:-----------:|:----------:|
| Core file/shell/search | ✅ | ✅ | ✅ |
| Multi-provider LLM | ✅ (6) | ❌ (1) | ✅ (65+) |
| Streaming + markdown | ✅ | ✅ | ✅ |
| Sub-agent delegation | ✅ | ✅ | ✅ |
| Dynamic tool creation | ✅ | ❌ | ✅ |
| Proxy support | ✅ | ❌ | ✅ |
| Zero-dependency binary | ✅ | ❌ | ❌ |
| MCP protocol | ❌ | ✅ | ✅ |
| Plugin/hook system | ❌ | ✅ | ✅ |
| IDE integration | ❌ | ✅ | ❌ |
| Browser automation | ❌ | ❌ | ✅ |
| Desktop automation | ❌ | ❌ | ✅ |
| Agent teams (parallel) | ❌ | ✅ | ✅ |
| Image analysis | ❌ | ✅ | ✅ |
| Session management | Partial | ✅ | ✅ |
| Auto-memory | Partial | ✅ | ❌ |
| Context compression | ❌ | ✅ | ❌ |
| Headless/CI mode | ❌ | ✅ | ✅ |
| Prompt caching | ❌ | ✅ | ✅ |
| Skills/marketplace | ❌ | ❌ | ✅ |
