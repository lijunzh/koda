# Koda 🐻

A high-performance, locally-focused AI coding agent built in Rust.

Single compiled binary. Multi-provider LLM support. Zero runtime dependencies.

## Install

### From crates.io
```bash
cargo install koda-agent
```

### From source
```bash
git clone https://github.com/lijunzh/koda.git
cd koda
cargo build --release
# Binary at ./target/release/koda
```

No extra setup needed — the default agent configuration is embedded
in the binary at compile time. On first run, an onboarding wizard
guides you through provider and API key setup.

## Quick Start

```bash
# Start with LM Studio (auto-detects the currently serving model)
koda

# Or with a cloud provider
koda --provider openai
koda --provider anthropic
```

## Features

- **Streaming responses** — tokens appear as they arrive, no waiting
- **Markdown rendering** — headers, bold, code blocks rendered inline during streaming
- **Syntax highlighting** — code blocks highlighted with `syntect` (same engine as `bat`)
- **Smart input** — tab completions, ghost hints, `@file` context injection
- **Arrow-key menus** — interactive model/provider pickers with ↑↓ navigation
- **Colored tool banners** — 🍯 Thinking, Response, and tool calls each with distinct colors
- **Compact tool output** — directory trees, grep summaries, file stats (not raw dumps)
- **18 built-in tools** — file ops, search, shell, web fetch, memory, task tracking, and more
- **Dynamic tool creation** — teach Koda new tools at runtime via `CreateTool`
- **Multi-provider LLM** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok
- **Durable execution** — SQLite-backed session history survives crashes
- **Session management** — list, resume, and delete past sessions
- **Persistent memory** — project & global memory injected into every conversation
- **Claude Code compatible** — reads `CLAUDE.md` and `AGENTS.md` for memory
- **Context window tracking** — usage shown in footer, warning in prompt at ≥75%
- **Token cost tracking** — `/cost` shows cumulative usage per session
- **Clipboard integration** — `/copy` code blocks, `/paste` from clipboard
- **Sub-agent orchestration** — delegate tasks via `InvokeAgent`
- **Safe path validation** — prevents directory traversal attacks
- **Onboarding wizard** — guided first-run setup
- **Version checker** — non-blocking update hints on startup

## Built-in Tools

Tools use PascalCase naming:

| Tool | Color | Description |
|------|-------|-------------|
| `Read` | Steel blue | Read file contents (with line-range support) |
| `Write` | Amber | Create/overwrite files |
| `Edit` | Amber | Targeted replacements (supports multi-edit) |
| `Delete` | Crimson | Delete files or directories (recursive) |
| `List` | Sky blue | List directory tree (respects .gitignore) |
| `Grep` | Silver | Recursive text search |
| `Glob` | Silver | Find files by pattern (`**/*.rs`) |
| `Bash` | Orange | Execute shell commands |
| `WebFetch` | Sky blue | Fetch & strip HTML from URLs |
| `TodoRead` | Silver | Read task list |
| `TodoWrite` | Amber | Create/update tasks |
| `MemoryRead` | Silver | Read project & global memory |
| `MemoryWrite` | Amber | Save insights to memory |
| `InvokeAgent` | Ruby | Delegate to a sub-agent |
| `ListAgents` | — | List available sub-agents |
| `CreateTool` | Violet | Define a new custom tool |
| `ListTools` | Silver | List custom tools |
| `DeleteTool` | Crimson | Remove a custom tool |

## REPL Commands

| Command | Description |
|---------|-------------|
| `/copy` | Copy last response or code block to clipboard |
| `/cost` | Show token usage for this session |
| `/help` | Command palette (select & execute) |
| `/memory` | View/save project & global memory |
| `/memory add <text>` | Save to project `MEMORY.md` |
| `/memory global <text>` | Save to global `~/.config/koda/memory.md` |
| `/model` | Pick a model (↑↓ arrow keys) |
| `/paste` | Show clipboard contents |
| `/provider` | Pick a provider |
| `/proxy <url>` | Set HTTP proxy (persisted) |
| `/sessions` | List recent sessions |
| `/sessions delete <id>` | Delete a session |
| `/quit` | Exit |

## Smart Input

```
🐻 [gpt-4o] ~/repo ❯ /mod         ← tab-completes to /model
🐻 [gpt-4o] ~/repo ❯ /model gpt   ← ghost hint: -4o-mini
🐻 [gpt-4o] ~/repo ❯ @src/main.rs ← injects file into context
```

## Visual Output

```
🐻 [gpt-4o] ~/repo ❯ what does this repo do?

  🍯 Thinking... (3s)

  ● List . (recursive)                          ← compact directory tree
  │ Cargo.toml
  │ README.md
  │ 📁 src/ (20 files, 2 subdirs)
  │ 📁 tests/ (5 files)
  │ (45 files, 5 dirs total)

  ● Read src/main.rs                             ← summary only
  │ 95 lines (4200 chars)

  ● Search src/ for 'TODO'                       ← grouped by file
  │ 📄 src/inference.rs (2 matches)
  │ 📄 src/app.rs (1 match)
  │ Found 3 matches across 2 files

  ● Response                                     ← vivid green
  Here's the architecture...

  1234 tokens · 5.2s · 237 t/s · context: 4.1k/128k (3%)

🐻 [gpt-4o] ~/repo ❯                              ← clean prompt

  ... many turns later ...

🐻 [gpt-4o] ~/repo (⚠ 82% context) ❯               ← context warning
```

## Memory System

Koda maintains persistent memory across sessions:

- **Project memory** — `MEMORY.md` in the project root (also reads `CLAUDE.md`, `AGENTS.md` for compatibility)
- **Global memory** — `~/.config/koda/memory.md` for user-wide preferences
- Both are injected into the system prompt automatically
- The LLM can save insights via `MemoryWrite`, or use `/memory add`

## Dynamic Tool Creation

Koda can teach itself new tools at runtime:

```
🐻 Create a tool called GitLog that shows recent commits

 ● Create tool: GitLog
Created custom tool 'GitLog'.
Template: git log --oneline -{{count}}

🐻 Show me the last 5 commits

 ● Shell $ git log --oneline -5
```

## Creating Custom Agents

Drop a JSON file in any agent directory (searched in order):

1. `<project_root>/agents/` — repo-local
2. Next to the `koda` binary — distributions
3. `~/.config/koda/agents/` — user-level

```json
{
    "name": "frontend",
    "system_prompt": "You are a frontend specialist...",
    "allowed_tools": ["Read", "Write", "List", "Grep", "Bash"],
    "model": "gpt-4o"
}
```

## Configuration

All config lives under `~/.config/koda/`:

| Path | Purpose |
|------|---------|
| `~/.config/koda/agents/` | User-level agent configs |
| `~/.config/koda/keys.toml` | API keys (chmod 600) |
| `~/.config/koda/memory.md` | Global memory |
| `~/.config/koda/history` | REPL command history |
| `~/.config/koda/todo.md` | Shared task list |

Project-local state:

| Path | Purpose |
|------|---------|
| `.koda.db` | SQLite session/conversation DB |
| `.koda_logs/` | Debug log files |
| `MEMORY.md` | Project memory (also reads `CLAUDE.md`, `AGENTS.md`) |
| `agents/` | Project-specific agents |
| `agents/tools/` | Custom tools (via CreateTool) |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `KODA_BASE_URL` | LLM provider base URL |
| `KODA_MODEL` | Default model name |
| `KODA_PROVIDER` | Default provider |
| `KODA_ACCEPT_INVALID_CERTS` | Accept self-signed certs (`1`/`true`) |
| `OPENAI_API_KEY` | OpenAI API key |
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `GEMINI_API_KEY` | Gemini API key |
| `GROQ_API_KEY` | Groq API key |
| `XAI_API_KEY` | Grok API key |

## CLI Options

```
koda [OPTIONS]

  -a, --agent <AGENT>          Agent to use [default: default]
  -s, --session <SESSION>      Session ID to resume
  -p, --project-root <PATH>    Project root [default: .]
      --base-url <URL>         LLM provider base URL
      --model <MODEL>          Model name
      --provider <PROVIDER>    Provider name
```

## Architecture

See [DESIGN.md](DESIGN.md) for the full architecture document.
See [FUTURE.md](FUTURE.md) for the roadmap and competitive analysis.

## Development

```bash
cargo test          # 239 tests across 6 suites
cargo build         # Debug build
cargo run           # Run locally
```

## License

MIT
