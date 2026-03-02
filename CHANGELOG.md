# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [0.1.0] - 2025-03-01

**A high-performance AI coding agent built in Rust.** Single binary. Multi-provider. Zero dependencies.

✨ **Highlights:** 14 tools · 6 LLM providers · 5 embedded agents · parallel sub-agents · image analysis · headless mode · auto-compact · prompt caching

### Core
- 14 built-in tools: file ops (Read/Write/Edit/Delete/List), search (Grep/Glob),
  shell (Bash), web (WebFetch), memory (MemoryRead/Write),
  agents (InvokeAgent/ListAgents/CreateAgent)
- 6 LLM providers: LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok
- Streaming responses with inline markdown rendering and syntax highlighting
- SQLite-backed durable sessions with session management
- Smart input: tab completion, ghost hints, @file context injection
- Interactive TUI menus for model/provider selection
- Tool confirmation system for destructive operations
- Safe path validation preventing directory traversal attacks
- Onboarding wizard for first-run setup

### Agents
- 5 built-in agents embedded at compile time (zero disk dependency):
  - **default** — main coding assistant
  - **reviewer** — critical code reviewer (read-only, severity-tagged)
  - **security** — security auditor (OWASP, CWE-tagged)
  - **testgen** — QA engineer (finds gaps, writes tests)
  - **releaser** — Release engineer (discover → plan → execute workflow)
- Agent discovery merges built-in + user (~/.config/koda/agents/) + project (agents/)
- CreateAgent tool with validation (name, duplicates, prompt quality)
- `/agent` command to list available sub-agents with source tags

### Parallel Execution
- Parallel tool execution via `futures::join_all` when no confirmation needed
- Enables concurrent sub-agent invocation (reviewer + security + testgen)

### Image Analysis
- Multi-modal image support via `@image.png` references
- Drag-and-drop: auto-detects bare image paths (absolute, ~/,  ./,  quoted)
- Provider-specific formats: OpenAI image_url data URIs, Anthropic image blocks
- Supported formats: PNG, JPEG, GIF, WebP, BMP

### Context Management
- Auto-compact at 80% context usage (LLM summarizes conversation)
- Manual `/compact` command
- Sliding window safety net for token budget
- Context percentage shown in prompt at ≥75%

### Headless Mode
- `koda -p "prompt"` or `koda "prompt"` for one-shot execution
- Piped stdin auto-detection: `echo "explain" | koda`
- `--output-format json` for CI/CD integration
- Exit code 0/1 for scripting

### Shell Output Management
- Display: last 20 lines shown to user (clean, predictable)
- LLM context: last 256 lines kept (sufficient for analysis)
- Per-line cap: 256 characters
- Tool description teaches LLM to pipe and filter output

### Git Integration
- `/diff` — show uncommitted changes summary
- `/diff review` — LLM code review of uncommitted changes
- `/diff commit` — generate conventional commit messages

### Performance
- Anthropic prompt caching: system prompt + tools cached (90% cheaper input tokens)
- Lean system prompt: 807 tokens (tool guidance lives in tool descriptions)
- Sub-agent prompts optimized: principle-based, language-agnostic

### Memory & Persistence
- Project memory: MEMORY.md (also reads CLAUDE.md, AGENTS.md)
- Global memory: ~/.config/koda/memory.md
- Persistent API keys: ~/.config/koda/keys.toml
- REPL history: ~/.config/koda/history

### REPL Commands
- `/agent` — list available sub-agents
- `/compact` — summarize conversation to reclaim context
- `/cost` — show token usage for the session
- `/diff` — show/review uncommitted changes, generate commit messages
- `/help` — command palette
- `/memory` — view/save project and global memory
- `/model` — switch models interactively
- `/provider` — switch LLM providers
- `/sessions` — list, resume, or delete sessions
- Keyboard shortcuts: `Ctrl+C` to interrupt, `Ctrl+D` to exit, `@file` to attach context

### CI/CD
- GitHub Actions: CI (fmt, clippy, test ×3 OS, doc, audit)
- Release pipeline: tag → CI gate → build (5 targets) → crates.io + GitHub release
- Release notes auto-extracted from CHANGELOG.md
- Dependabot for cargo and GitHub Actions dependencies

### Testing
- 288 tests across 6 suites
- All CI checks passing: cargo fmt, clippy -D warnings, test, doc
