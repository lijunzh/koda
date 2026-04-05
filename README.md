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
- **Auto-provisioned capabilities.** Beyond the core, koda ships first-party
  library integrations (AST analysis, email) that activate on demand. You
  never configure plumbing.
- **Extensible architecture.** First-party capabilities are direct library
  calls for speed and reliability. Each also ships as a standalone MCP
  server for use in other editors.
- **Ask Koda what it can do.** Just ask — "what can you do?" Koda's
  capabilities are embedded in its system prompt, so it can always describe
  its own tools, commands, and features accurately.

## Install

```bash
# Homebrew (macOS / Linux)
brew tap lijunzh/koda
brew install koda

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
koda -p "fix the bug in auth.rs"  # Headless one-shot
echo "explain this" | koda        # Piped input
```

## Features

- **20+ built-in tools** — file ops, search, shell, web fetch, memory, agents, AST analysis, email, context recall
- **First-party integrations** — AST analysis (tree-sitter) and email (IMAP/SMTP) as direct library calls; also available as standalone MCP servers
- **14 LLM providers** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok, Ollama, DeepSeek, Mistral, MiniMax, OpenRouter, Together, Fireworks, vLLM
- **User-defined agents** — create specialized agents via JSON configs (testgen, releaser, planner, etc.)
- **Smart context** — queries context window from provider API at startup (falls back to lookup table), rate limit retry with backoff, auto-compact
- **Git checkpointing** — auto-snapshots before each turn for safe rollback
- **Approval modes** — auto (default) / confirm (confirm writes) via `Shift+Tab`
- **Per-tool safety gates** — destructive ops and outside-project writes always need confirmation; local mutations auto-approved in auto mode
- **File ownership tracking** — files created by koda in a turn can be auto-approved for deletion in the same turn (no double-confirmation)
- **Folder-scoped permissions** — writes outside `project_root` always require confirmation; bash commands with path escapes are flagged
- **Diff preview** — see exactly what changes before approving Edit, Write, Delete
- **Loop detection** — catches repeated tool calls with configurable iteration caps
- **Parallel execution** — concurrent tool calls and sub-agent orchestration
- **Extended thinking** — structured thinking block display with configurable budgets
- **Image analysis** — `@image.png` or drag-and-drop for multi-modal input
- **Fullscreen TUI** — alternate screen buffer with app-managed scrollback, mouse scroll during inference, native clipboard copy
- **Git integration** — `/diff` review, commit message generation
- **Headless mode** — `koda -p "prompt"` with JSON output for CI/CD
- **Persistent memory** — project (`MEMORY.md`) and global (`~/.config/koda/memory.md`)
- **Skills** — built-in expertise modules (code review, security audit) + user-created skills for repeatable analysis

### 📚 Skills

Skills inject expert instructions into context — zero cost, instant activation.
Built-in: `code-review`, `security-audit`. Create your own by adding a
`SKILL.md` file to `.koda/skills/<name>/` (project) or `~/.config/koda/skills/<name>/` (global).
Use `/skills` to browse, or ask Koda to "use the code review skill."

### 🌳 AST & 📧 Email

Koda natively understands code structure (Rust, Python, JS, TS via tree-sitter)
and connects to email (IMAP/SMTP). Both are auto-provisioned — just ask.
Both are auto-provisioned — just ask.

## REPL Commands

| Command | Description |
|---------|-------------|
| `/help` | Command palette (select & execute) |
| `/agent` | List available sub-agents |
| `/compact` | Summarize conversation to reclaim context |
| `/diff` | Show/review uncommitted changes |
| `/key` | Manage API keys |
| `/memory` | View/save project & global memory |
| `/model` | Pick a model (curated aliases + local) |
| `/provider` | Browse all models from a provider |
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
| **Shift+Tab** | At prompt | Cycle mode (auto ↔ confirm) |
| **Ctrl+D** | At prompt (empty) | Exit Koda |
| **↑/↓** | At prompt | Browse command history |
| **Mouse scroll** | History panel | Scroll output (works during inference) |
| **Click+drag** | History panel | Select text for clipboard copy |

## Architecture

Koda is a Cargo workspace with four crates:

```
koda/
├── koda-core/     # Engine library (providers, tools, inference, DB) — zero terminal deps
├── koda-cli/      # CLI binary (REPL, display, approval UI)
├── koda-ast/      # Tree-sitter AST analysis (library + standalone MCP server)
└── koda-email/    # Email via IMAP/SMTP (library + standalone MCP server)
```

The engine communicates through `EngineEvent` (output) and `EngineCommand` (input) enums
over async channels. See [docs/design.md](docs/design.md) for architectural decisions.

## Custom Agents

Define specialized agents as JSON files in `agents/`. Sub-agents can use
different models for cost optimization:

```json
// agents/testgen.json
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist.",
  "model": "gemini-2.5-flash",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

See [docs.rs/koda-core](https://docs.rs/koda-core) for full config reference.

## Documentation

| Document | Audience | Content |
|---|---|---|
| [**API Reference**](https://docs.rs/koda-core) | Users / Contributors | Auto-generated from code |
| [**Design**](docs/design.md) | Contributors | Principles, architecture |
| [**CLAUDE.md**](CLAUDE.md) | AI / Contributors | Workspace layout, conventions |
| [**Changelog**](CHANGELOG.md) | Everyone | Version history |

## Development

```bash
cargo test --workspace --features koda-core/test-support  # Run all tests
cargo clippy --workspace      # Lint
cargo run -p koda-cli         # Run locally
```

## License

MIT
