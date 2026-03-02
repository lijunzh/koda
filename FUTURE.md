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

## Medium Features (v0.1.x) — ALL COMPLETE ✅

All medium features were completed before the v0.1.0 release:

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

### 2.4. ~~Notebook Support (Jupyter)~~ (Skipped)

**Status:** Skipped. Low priority for a CLI-first coding agent. Users who
need notebook editing have better tools (JupyterLab, VS Code).

**Effort:** Medium.

**What:** Read and edit `.ipynb` Jupyter notebook files. Parse the JSON
structure to show cell contents, and allow editing individual cells.

**Reference:** Claude Code has `NotebookRead` and `NotebookEdit`.

---

### 2.5. Headless / Non-Interactive Mode ✅

**Status:** Implemented. Multiple invocation styles:
- `koda -p "prompt"` — flag-based
- `koda "prompt"` — positional argument
- `echo "prompt" | koda` — auto-detects piped stdin
- `koda -p -` — explicit stdin read
- `--output-format json` — structured output for CI/CD

Skips banner, onboarding, version check. Tools still work. Returns exit
code 0/1 for scripting.

**Effort:** Medium.

**What:** Run Koda as a one-shot CLI tool:
```bash
koda "fix the login bug in src/auth.rs" --headless
```
Execute the task and exit without entering the REPL. Useful for CI/CD
pipelines, scripts, and editor integrations.

**Reference:** Claude Code supports `claude -p "prompt"` mode.

---

### 2.6. ~~Ask User Question — Interactive Multi-Choice TUI~~ (Skipped)

**Status:** Skipped. Claude Code — the most capable coding agent — doesn't
have this. The LLM already asks questions in plain text and parses freeform
responses just fine. YAGNI.

**Effort:** Medium.

**What:** A tool the LLM can call to ask the user a structured question
with multiple-choice options, rendered as an arrow-key selector.

**Reference:** Code Puppy has `ask_user_question` with a full TUI.

---

### 2.7. ~~Model Marketplace Integration~~ (Skipped)

**Status:** Skipped. Koda already supports 6 providers, and any OpenAI-compatible
endpoint works via `--base-url`. Developers know what model they want. Claude Code
doesn't have this either. Adding new providers is a 5-line code change, not a feature.

**Effort:** Medium.

**What:** Browse and add models from an API catalog (e.g., models.dev)
via an interactive `/add_model` command.

**Reference:** Code Puppy integrates with models.dev (65+ providers).

---

### 2.8. Prompt Caching (Anthropic) ✅

**Status:** Implemented. System prompt and tool definitions are sent with
`cache_control: {type: "ephemeral"}` markers. Anthropic caches the static
prefix (~3,500–4,500 tokens) and serves it at 90% lower cost on subsequent
calls. Cache stats logged at debug level. Beta header included.

**Effort:** Medium.

**What:** Use Anthropic's prompt caching API to cache the system prompt
and reduce costs/latency for repeated interactions.

**Reference:** Both Claude Code and Code Puppy support prompt caching.

---

## 3. Road to v0.2.0 (Large Features)

Significant architectural work and differentiating capabilities that form the roadmap for `v0.2.0`.

### 3.1. Concurrent TUI with Non-Blocking Input

**Effort:** Large. This is the next major architectural evolution.

**Vision:** Separate the input loop from the execution loop so the user
can type new prompts, run commands, and queue tasks while Koda is
thinking or executing tools. Inspired by Claude Code's footer-based UX.

**Why Rust makes this possible:** Python agents (Code Puppy) are limited
by the GIL — they can't truly separate input handling from inference.
Rust's async (tokio) + crossterm gives us real concurrent I/O: streaming
LLM responses on one task while reading user input on another.

**Architecture:**
```
┌──────────────────────────────────────────────────────┐
│  Scrollable output area                              │
│  (streaming LLM response, tool output, agent results) │
│                                                       │
│  ● Read src/main.rs                                   │
│  │ 95 lines (4200 chars)                              │
│                                                       │
│  ● Response                                           │
│  Here's the architecture...                           │
│                                                       │
├──────────────────────────────────────────────────────┤
│ > type your next prompt here...                       │
├──────────────────────────────────────────────────────┤
│ /help · claude-sonnet-4 · ctx: 12% · ⚡ 2 tasks running │
└──────────────────────────────────────────────────────┘
```

**Three regions:**
1. **Output area** (scrollable) — streaming responses, tool banners, agent results
2. **Input line** (always active) — user can type while output is streaming
3. **Footer bar** (persistent) — shortcuts, model name, context %, active task count

**Parallel Execution UX (The "Claude Code" pattern):**
Parallel tool execution (e.g., 3 sub-agents running at once) breaks traditional
streaming output. The TUI will solve this using **collapsible task groups**:
- **While running:** Show a live-updating list of spinners (e.g., `⠧ security: scanning...`)
- **When finished:** Collapse into a single summary line (`▶ 3 tools executed`)
- **Interactive:** User can arrow-key up to the summary and hit `Enter` to expand
  and view the raw output. We will explicitly avoid Tmux-style vertical splits,
  as they don't scale when the LLM invokes 5+ tools simultaneously.

**Key capabilities this unlocks:**
- **Type while thinking** — queue next prompt while LLM is responding
- **Interrupt and redirect** — Ctrl+C stops current task, immediately accept new input
- **Parallel task visibility** — footer shows "⚡ 3 tasks running" during parallel agents
- **Scroll back** — review earlier output without losing the input line
- **Slash commands during execution** — `/cost` or `/compact` while a task runs

**Implementation approach:**
- Use `ratatui` with `crossterm` backend (crossterm already a dependency)
- Two tokio tasks: input handler + inference/tool executor
- Communication via `tokio::sync::mpsc` channels
- Input task sends: UserPrompt, SlashCommand, Interrupt
- Executor task sends: StreamChunk, ToolBanner, ToolOutput, Done
- Render loop consumes events from both channels and updates the TUI

**Migration path (incremental, not big-bang):**
1. First: add a persistent footer bar (model, context %, shortcuts)
   — minimal ratatui, keep current streaming output
2. Then: separate input into its own tokio task with channel
   — user can type during inference
3. Then: scrollable output area
   — full ratatui alternate screen
4. Finally: parallel task queue with visibility
   — multiple prompts in flight

**Subsumes:**
- Transcript fold/unfold (removed in v0.1.0)
- The current streaming-print approach (replaced by render loop)
- Progress bars for long-running tool calls

**Dependencies:** `ratatui` crate (~zero new deps since crossterm is already included)

**Reference:** Claude Code has a footer bar with shortcuts and model info.
Code Puppy cannot do this due to Python GIL limitations.

---

### 3.2. MCP Protocol (Model Context Protocol)

**Effort:** Large.

**What:** Support the [Model Context Protocol](https://modelcontextprotocol.io/)
for extensible tool servers. MCP allows third-party tools to be exposed
to the LLM via a standardized JSON-RPC protocol.

**Why it matters:** This is currently Koda's largest feature gap compared
to Claude Code and Code Puppy. MCP is becoming the industry standard for
AI tool extensibility (Linear, GitHub, Postgres, etc.). Without it, Koda
is limited to its 14 built-in tools.

**Approach:**
- Implement MCP client (stdio and SSE transports)
- Auto-discover tools from connected servers
- Merge MCP tools into Koda's existing tool registry

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

**Phase 1: Parallel tool execution ✅**

Implemented. When the LLM returns multiple tool calls in one response
and none require user confirmation, they run concurrently via
`futures::join_all`. This covers the primary use case: parallel
read-only agents (code review + QA + security audit simultaneously).

- `can_parallelize()` checks if any tool needs confirmation
- Safe tools (Read, Grep, List, InvokeAgent, etc.) run in parallel
- Confirmation-required tools (Write, Edit, Delete, Bash) force sequential
- Results stored in original order for deterministic conversation flow

**Phase 2: Git worktree isolation (Future)**

**Effort:** Large. Only needed for parallel WRITE operations.

When multiple agents need to modify files simultaneously, each agent
would get its own git worktree (a separate checkout of the same repo)
to prevent conflicts. After all agents complete, branches are merged.

Scope:
- Create/cleanup worktrees per agent (`git worktree add/remove`)
- Each agent gets a separate `project_root` and `ToolRegistry`
- Automatic branch creation per agent task
- Post-completion merge with conflict detection
- Conflict resolution: show user the diff and let them/LLM decide
- Handle edge cases: dirty working tree, submodules, shallow clones

Prerequisites: Real usage patterns showing demand for parallel writes.
Phase 1 covers ~90% of parallel agent use cases.

**Reference:** Claude Code has agent teams with worktree isolation.
Code Puppy has a "pack" system (bloodhound, husky, retriever, etc.).

---

### 3.6. Browser Automation

**Effort:** Large.

**What:** Playwright-based browser control for web testing, scraping,
and UI interaction. 30+ tools for navigation, clicks, form filling,
screenshots, and workflow recording.

**Why it matters:** Koda currently relies purely on static code analysis
and shell commands. Browser automation allows agents to visually inspect
and interact with the web UIs they build, closing a major capability gap
with Code Puppy.

**Reference:** Code Puppy has full browser automation.

---

### 3.7. ~~Specialized Reviewer Agents~~ ✅

**Status:** Implemented. Four pre-built agents ship with Koda:

- **`reviewer`** — Critical code reviewer (bugs, patterns, design issues).
  Read-only tools. Severity-tagged output (🔴 Bug, 🟡 Warning, 🔵 Suggestion).
- **`security`** — Paranoid security auditor (OWASP, CVEs, secrets, injection).
  CWE-tagged findings. Executive risk summary.
- **`testgen`** — QA engineer and test writer. Finds coverage gaps,
  writes actual test code. Full tool access to create test files.
- **`releaser`** — Release engineer for GitHub releases. Exact workflow:
  tests → version bump → changelog → commit → tag → push → `gh release`.

Language-specific reviewers (Python, Rust, JS/TS) were skipped —
the general reviewer handles multiple languages well enough.

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
| Agent teams (parallel) | ✅ (Phase 1) | ✅ | ✅ |
| Image analysis | ✅ | ✅ | ✅ |
| Session management | ✅ | ✅ | ✅ |
| Auto-memory | ✅ | ✅ | ❌ |
| Context compression | ✅ (auto) | ✅ | ❌ |
| Headless/CI mode | ✅ | ✅ | ✅ |
| Prompt caching | ✅ | ✅ | ✅ |
| Skills/marketplace | ❌ | ❌ | ✅ |
