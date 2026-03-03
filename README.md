# Koda 🐻

A high-performance AI coding agent built in Rust.

Single compiled binary. Multi-provider LLM support. Zero runtime dependencies.

## Philosophy

**Koda is a personal coding agent.** It's built for a single developer at a keyboard,
not for enterprise teams or platform integrations. This focus drives every design decision:

- **Single binary, zero runtime deps.** `cargo install` and you're done. No Node.js,
  no Python, no Docker. Works offline with local models (LM Studio) or online with
  cloud providers.
- **Built-in tools for the core coding loop.** File ops, search, shell, web fetch,
  memory, and agents are compiled in — always available, zero latency, zero config.
- **MCP for everything else.** Need GitHub API, databases, Slack? Connect external
  MCP servers via `.mcp.json`. Koda stays lean; the ecosystem handles the long tail.
- **Ask Koda what it can do.** Just ask — "what can you do?" or "what tools do you
  have?" Koda's capabilities are embedded in its system prompt, so it can always
  describe its own tools, commands, and features accurately.

## Install

```bash
# From crates.io
cargo install koda-agent

# From source
git clone https://github.com/lijunzh/koda.git
cd koda && cargo build --release
```

On first run, an onboarding wizard guides you through provider and API key setup.

## Quick Start

```bash
koda                              # Interactive REPL (auto-detects LM Studio)
koda --provider anthropic         # Use a cloud provider
koda -p "fix the bug in auth.rs"  # Headless one-shot
echo "explain this" | koda        # Piped input
```

## What's Inside

- **16 built-in tools** — file ops, search, shell, web fetch, memory, agents, task tracking
- **MCP support** — connect to any [MCP server](https://modelcontextprotocol.io) via `.mcp.json` (same format as Claude Code / Cursor)
- **6 LLM providers** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok
- **5 embedded agents** — default, code reviewer, security auditor, test writer, release engineer
- **Approval modes** — plan (read-only) / normal (smart confirm) / yolo (auto-approve) via `/trust`
- **Diff preview** — see exactly what changes before approving Edit, Write, Delete
- **Loop detection** — catches repeated tool calls with configurable iteration caps
- **Parallel execution** — concurrent tool calls and sub-agent orchestration
- **Smart context** — auto-compact (configurable threshold), sliding window, prompt caching (Anthropic)
- **Extended thinking** — structured thinking block display with configurable budgets
- **Image analysis** — `@image.png` or drag-and-drop for multi-modal input
- **Git integration** — `/diff` review, commit message generation
- **Headless mode** — `koda -p "prompt"` with JSON output for CI/CD
- **Persistent memory** — project (`MEMORY.md`) and global (`~/.config/koda/memory.md`)

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
| `/trust` | Switch approval mode (plan/normal/yolo) |
| `/exit` | Quit Koda |

**Tips:** `@file` to attach context · `Shift+Tab` to cycle trust mode · `Esc` to clear input

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

## Documentation

- **[DESIGN.md](DESIGN.md)** — Architecture, technical stack, component breakdown
- **[FUTURE.md](FUTURE.md)** — Roadmap and competitive analysis
- **[CHANGELOG.md](CHANGELOG.md)** — Release history and detailed feature list

## Development

```bash
cargo test          # Run all tests
cargo clippy        # Lint
cargo run           # Run locally
```

See [FUTURE.md](FUTURE.md) for the full roadmap.

## License

MIT
