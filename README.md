# Koda 🐻

A high-performance AI coding agent built in Rust.

Single compiled binary. Multi-provider LLM support. Zero runtime dependencies.

## Philosophy

**Koda is a personal AI assistant.** Coding is the starting point, but the platform
will expand to support email, calendar, knowledge management, and more — all
powered by the same engine. This focus drives every design decision:

- **Everything just works.** `cargo install koda-cli` and you're done.
  No Node.js, no Python, no Docker. Core tools (file ops, search, shell, web
  fetch, memory, agents) are compiled in — always available, zero config.
- **Auto-provisioned capabilities.** Beyond the core, koda auto-installs
  MCP servers on demand. Ask about your email? Koda installs the email
  integration transparently. You never configure plumbing.
- **MCP is the extension model.** Need GitHub API, databases, Slack? Connect
  external MCP servers via `.mcp.json`, or let koda auto-discover them.
  Koda stays lean; the ecosystem handles the long tail.
- **Ask Koda what it can do.** Just ask — "what can you do?" Koda's
  capabilities are embedded in its system prompt, so it can always describe
  its own tools, commands, and features accurately.

## Install

```bash
# From crates.io
cargo install koda-cli

# From source
git clone https://github.com/lijunzh/koda.git
cd koda && cargo build --release -p koda-cli
# Binary is at target/release/koda
```

On first run, an onboarding wizard guides you through provider and API key setup.

## Quick Start

```bash
koda                              # Interactive REPL (auto-detects LM Studio)
koda --provider anthropic         # Use a cloud provider
koda --skip-probe                 # Skip model capability probe at startup
koda -p "fix the bug in auth.rs"  # Headless one-shot
echo "explain this" | koda        # Piped input
```

## Features

- **20+ built-in tools** — file ops, search, shell, web fetch, memory, agents, AST analysis, email, context recall
- **MCP support** — connect to any [MCP server](https://modelcontextprotocol.io) via `.mcp.json` (same format as Claude Code / Cursor)
- **14 LLM providers** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok, Ollama, DeepSeek, Mistral, MiniMax, OpenRouter, Together, Fireworks, vLLM
- **User-defined agents** — create specialized agents via JSON configs (testgen, releaser, planner, etc.)
- **Model probe** — one-time structured output test at startup to verify model capabilities
- **Smart context** — queries context window from provider API at startup (falls back to lookup table), rate limit retry with backoff, auto-compact
- **Git checkpointing** — auto-snapshots before each turn for safe rollback
- **Approval modes** — auto (default) / strict (confirm writes) / safe (read-only) via `Shift+Tab`
- **Per-tool safety gates** — destructive ops and outside-project writes always need confirmation; local mutations auto-approved in auto mode
- **Folder-scoped permissions** — writes outside `project_root` always require confirmation; bash commands with path escapes are flagged
- **Diff preview** — see exactly what changes before approving Edit, Write, Delete
- **Loop detection** — catches repeated tool calls with configurable iteration caps
- **Parallel execution** — concurrent tool calls and sub-agent orchestration
- **Extended thinking** — structured thinking block display with configurable budgets
- **Image analysis** — `@image.png` or drag-and-drop for multi-modal input
- **Git integration** — `/diff` review, commit message generation
- **Headless mode** — `koda -p "prompt"` with JSON output for CI/CD
- **Persistent memory** — project (`MEMORY.md`) and global (`~/.config/koda/memory.md`)
- **Cost tracking** — per-turn and per-session cost estimation including thinking tokens
- **Skills** — built-in expertise modules (code review, security audit) + user-created skills for repeatable analysis

### 📚 Skills

Skills inject expert instructions into context — zero cost, instant activation.
Koda includes built-in skills for common analysis tasks, and you can create your own.

**Built-in skills:**
- `code-review` — senior code review (bugs, anti-patterns, improvements)
- `security-audit` — security vulnerability scan (OWASP checklist)

**Create custom skills:** add a `SKILL.md` file with YAML frontmatter to:
- `.koda/skills/<name>/SKILL.md` — project-level (shared with team)
- `~/.config/koda/skills/<name>/SKILL.md` — user-level (global)

```markdown
---
name: my-skill
description: What this skill does
tags: [tag1, tag2]
---

# Instructions for the agent

Your expert guidance here...
```

Use `/skills` to list available skills, or ask Koda to "use the code review skill".

### 🌳 AST Code Analysis

Koda natively understands the structure of your codebase using embedded `tree-sitter` parsers.
- **Auto-provisioned:** just ask koda to analyze code structure — no setup needed.
- **Built-in languages:** Rust, Python, JavaScript, TypeScript — instant function/class extraction and call graphs.
- **Extending with MCP:** Need Go, C++, or Java? Connect a community Tree-sitter MCP server via `.mcp.json`.

### 📧 Email Integration

Koda connects to your email via IMAP/SMTP through the koda-email MCP server.
- **Auto-provisioned:** just ask "check my email" — koda sets it up.
- **Any provider:** Gmail, Outlook, FastMail, self-hosted.
- **Read, search, send:** full email workflow from the CLI.

## REPL Commands

| Command | Description |
|---------|-------------|
| `/help` | Command palette (select & execute) |
| `/agent` | List available sub-agents |
| `/compact` | Summarize conversation to reclaim context |
| `/cost` | Show token usage for this session |
| `/diff` | Show/review uncommitted changes |
| `/mcp` | MCP servers: status, add, remove, restart |
| `/memory` | View/save project & global memory |
| `/model` | Pick a model (↑↓ arrow keys) |
| `/provider` | Switch LLM provider |
| `/sessions` | List, resume, or delete sessions |
| `/skills` | List available skills (search with `/skills <query>`) |
| `/exit` | Quit Koda |

**Tips:** `@file` to attach context · Tab to autocomplete · `Shift+Tab` to cycle mode · `Alt+Enter` for multi-line

### Keyboard Shortcuts

| Key | Context | Action |
|-----|---------|--------|
| **Tab** | At prompt | Autocomplete (`/commands`, `@files`, `/model names`) |
| **Alt+Enter** | At prompt | Insert newline (multi-line input) |
| **Ctrl+C** | During inference | Cancel the current turn |
| **Ctrl+C ×2** | During inference | Force quit Koda |
| **Ctrl+C** | At prompt (with text) | Clear the line |
| **Esc** | At prompt | Clear the line |
| **Shift+Tab** | At prompt | Cycle mode (auto → strict → safe) |
| **Ctrl+D** | At prompt (empty) | Exit Koda |
| **↑/↓** | At prompt | Browse command history |

## MCP (Model Context Protocol)

Koda connects to external [MCP servers](https://modelcontextprotocol.io) for additional tools.
Create a `.mcp.json` in your project root (same format as Claude Code / Cursor):

```json
{
  "mcpServers": {
    "context7": {
      "command": "npx",
      "args": ["-y", "@upstash/context7-mcp"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "$GITHUB_TOKEN" }
    }
  }
}
```

Servers auto-connect on startup. MCP tools appear alongside built-in tools with
namespaced names (e.g. `github.create_issue`). Manage at runtime with `/mcp`.

User-level servers go in `~/.config/koda/mcp.json` (merged, project overrides).

## Architecture

Koda is a Cargo workspace with four crates:

```
koda/
├── koda-core/     # Engine library (providers, tools, inference, DB) — zero terminal deps
├── koda-cli/      # CLI binary (REPL, display, approval UI)
├── koda-ast/      # MCP server: tree-sitter AST analysis
└── koda-email/    # MCP server: email via IMAP/SMTP
```

The engine communicates through `EngineEvent` (output) and `EngineCommand` (input) enums
over async channels. See [DESIGN.md](DESIGN.md) for architectural decisions.

## Getting the Most Out of Koda

### Model capability probe

At session start, Koda sends a small structured output test to verify the model can produce valid tool calls. If the probe fails, Koda warns you and falls back to text-only mode. Skip with `--skip-probe`.

### Create custom agents

Define specialized agents as JSON files in your project's `agents/` directory:

```json
// agents/testgen.json — test generation specialist
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist. Write comprehensive tests.",
  "provider": "gemini",
  "model": "gemini-2.5-flash",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"],
  "max_iterations": 15
}
```

Sub-agents can run on different models for cost optimization. The default agent is built-in; all others are user-created.

### Context window management

Koda auto-detects your model's context window and manages it:

| Model | Context | Auto-compact at |
|-------|---------|----------------|
| Claude Opus/Sonnet | 200K tokens | 90% |
| Gemini 2.5 | 1M tokens | 80% |
| GPT-4o | 128K tokens | 90% |
| Local models | 4K–128K | 70% |

Use `/compact` manually, or let auto-compact handle it. The `/cost` command shows token usage and estimated cost.

## Documentation

- **[DESIGN.md](DESIGN.md)** — Design decisions and rationale
- **[CHANGELOG.md](CHANGELOG.md)** — Release history
- **[CLAUDE.md](CLAUDE.md)** — Developer guide for AI assistants
- **[GitHub Issues](https://github.com/lijunzh/koda/issues)** — Roadmap and release tracking

## Development

```bash
cargo test --workspace --features koda-core/test-support  # Run all 432 tests
cargo clippy --workspace      # Lint
cargo run -p koda-cli         # Run locally
```

## License

MIT
