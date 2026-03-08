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
koda --model-tier strong          # Force a specific tier (usually auto-adapts)
koda -p "fix the bug in auth.rs"  # Headless one-shot
echo "explain this" | koda        # Piped input
```

## Features

- **20+ built-in tools** — file ops, search, shell, web fetch, memory, agents, AST analysis, email, context recall
- **MCP support** — connect to any [MCP server](https://modelcontextprotocol.io) via `.mcp.json` (same format as Claude Code / Cursor)
- **14 LLM providers** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok, Ollama, DeepSeek, Mistral, MiniMax, OpenRouter, Together, Fireworks, vLLM
- **6 built-in agents** — default, test writer, release engineer, codebase scout, planner, verifier
- **Model-adaptive** — starts all models at Standard tier, then promotes to Strong or demotes to Lite based on observed tool-use quality
- **Lazy tool loading** — Strong models get 9 core tools; discover more on demand via `DiscoverTools`
- **Smart context** — queries context window from provider API at startup (falls back to lookup table), rate limit retry with backoff, auto-compact
- **Approval modes** — auto (default) / strict (confirm writes) / safe (read-only) via `Shift+Tab`
- **Diff preview** — see exactly what changes before approving Edit, Write, Delete
- **Loop detection** — catches repeated tool calls with configurable iteration caps
- **Parallel execution** — concurrent tool calls and sub-agent orchestration
- **Extended thinking** — structured thinking block display with configurable budgets
- **Image analysis** — `@image.png` or drag-and-drop for multi-modal input
- **Git integration** — `/diff` review, commit message generation
- **Headless mode** — `koda -p "prompt"` with JSON output for CI/CD
- **Persistent memory** — project (`MEMORY.md`) and global (`~/.config/koda/memory.md`)
- **Cost tracking** — per-turn and per-session cost estimation including thinking tokens

### 🌳 AST Code Analysis

Koda natively understands the structure of your codebase using embedded `tree-sitter` parsers.
- **Built-in languages:** Rust, Python, JavaScript, TypeScript — instant function/class extraction and call graphs.
- **Extending with MCP:** Need Go, C++, or Java? Connect a community Tree-sitter MCP server via `.mcp.json`.

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

### Model-Adaptive Architecture

Koda auto-detects your model's capabilities and adapts its behavior:

| Tier | Models | Behavior |
|------|--------|----------|
| **Strong** | Promoted at runtime after 3 successful tool-use turns | Minimal prompts, lazy tool loading, parallel execution |
| **Standard** | Default for all models | Full prompts, all tools, balanced |
| **Lite** | Demoted at runtime after 2+ hallucinated/malformed tool calls | Verbose prompts, step-by-step guidance |

Tier is observed at runtime, not guessed from model names. Override with `--model-tier strong|standard|lite` or `"model_tier": "strong"` in agent config.

## Getting the Most Out of Koda

### Model tiers adapt automatically

Koda starts every model at Standard and adapts based on observed behavior:
- **Promotion to Strong** — after 3 turns of valid tool calls (correct names, parseable JSON)
- **Demotion to Lite** — if 2+ tool calls hallucinate names or send malformed JSON

You can force a tier if needed:

```bash
koda --model-tier strong    # Minimal prompts, lazy tools (saves ~57% token overhead)
koda --model-tier lite      # Verbose prompts, step-by-step guidance for small models
```

The status bar shows your current tier: `claude-sonnet-4-6 [Standard]` (then `[Strong]` after promotion)

### Delegate with sub-agents

Koda ships with specialized agents. Use them for focused tasks:

| Agent | Purpose | Tools |
|-------|---------|-------|
| **scout** | Codebase exploration (read-only) | Read, List, Grep, Glob |
| **testgen** | Test generation | All tools |
| **planner** | Task decomposition (read-only) | Read, List, Grep, Glob |
| **verifier** | Quality verification | Read, Grep, Bash |
| **releaser** | Release engineering | All tools |

Koda's intent classifier suggests agents automatically: "find all uses of X" → scout, "write tests" → testgen.

Sub-agents can run on different models for cost optimization:
```json
// agents/scout.json — use cheap model for exploration
{
  "name": "scout",
  "provider": "gemini",
  "model": "gemini-2.5-flash",
  "allowed_tools": ["Read", "List", "Grep", "Glob"],
  "max_iterations": 10
}
```

### Context window management

Koda auto-detects your model's context window and manages it:

| Model | Context | Auto-compact at |
|-------|---------|----------------|
| Claude Opus/Sonnet | 200K tokens | 90% (Strong) |
| Gemini 2.5 | 1M tokens | 80% (Standard) |
| GPT-4o | 128K tokens | 90% (Strong) |
| Local models | 4K–128K | 70% (Lite) |

Use `/compact` manually, or let auto-compact handle it. The `/cost` command shows token usage and estimated cost.

### Save tokens with DiscoverTools

Strong-tier models load only core tools (Read, Write, Edit, etc.) by default. When the model needs agents, skills, or other capabilities, it calls `DiscoverTools` to load them on demand — saving ~57% per-turn tool overhead.

### Recall older context

If context was dropped from the sliding window, the model can use `RecallContext` to search or retrieve specific turns from conversation history.

## Documentation

- **[DESIGN.md](DESIGN.md)** — Design decisions and rationale
- **[CHANGELOG.md](CHANGELOG.md)** — Release history
- **[CLAUDE.md](CLAUDE.md)** — Developer guide for AI assistants
- **[GitHub Issues](https://github.com/lijunzh/koda/issues)** — Roadmap and release tracking

## Development

```bash
cargo test --workspace --features koda-core/test-support  # Run all 489 tests
cargo clippy --workspace      # Lint
cargo run -p koda-cli         # Run locally
```

## License

MIT
