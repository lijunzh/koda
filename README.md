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
# Start interactive REPL (auto-detects LM Studio if running)
koda

# With a cloud provider
koda --provider openai
koda --provider anthropic

# Headless mode — run a single prompt and exit
koda -p "fix the login bug in src/auth.rs"
koda "explain this codebase"

# Pipe from stdin
echo "explain this error" | koda
cat error.log | koda

# JSON output for CI/CD
koda -p "list all TODOs" --output-format json
```

## Features

- **Streaming responses** — tokens appear as they arrive, no waiting
- **Markdown rendering** — headers, bold, code blocks rendered inline during streaming
- **Syntax highlighting** — code blocks highlighted with `syntect` (same engine as `bat`)
- **Smart input** — tab completions, ghost hints, `@file` context injection
- **Image analysis** — `@image.png` or drag-and-drop images for multi-modal analysis
- **Arrow-key menus** — interactive model/provider pickers with ↑↓ navigation
- **Colored tool banners** — 🍯 Thinking, Response, and tool calls each with distinct colors
- **19 built-in tools** — file ops, search, shell, web fetch, memory, task tracking, and more
- **Dynamic tool creation** — teach Koda new tools at runtime via `CreateTool`
- **Multi-provider LLM** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok
- **Prompt caching** — Anthropic prompt caching for 90% cheaper input tokens
- **Durable execution** — SQLite-backed session history survives crashes
- **Session management** — list, resume, and delete past sessions
- **Persistent memory** — project & global memory injected into every conversation
- **Claude Code compatible** — reads `CLAUDE.md` and `AGENTS.md` for memory
- **Context window management** — auto-compact at 80%, manual `/compact`, sliding window
- **Token cost tracking** — `/cost` shows cumulative usage per session
- **Clipboard integration** — `/copy` code blocks, `/paste` from clipboard
- **Sub-agent orchestration** — delegate tasks via `InvokeAgent` with parallel execution
- **4 pre-built agents** — code reviewer, security auditor, test writer, release engineer
- **Headless mode** — `koda -p "prompt"` for CI/CD, scripting, and piped input
- **Git integration** — `/diff` review, commit message generation
- **Safe path validation** — prevents directory traversal attacks
- **Onboarding wizard** — guided first-run setup

## Built-in Tools

Tools use PascalCase naming:

| Tool | Description |
|------|-------------|
| `Read` | Read file contents (with line-range support) |
| `Write` | Create/overwrite files |
| `Edit` | Targeted replacements (supports multi-edit) |
| `Delete` | Delete files or directories (recursive) |
| `List` | List directory tree (respects .gitignore) |
| `Grep` | Recursive text search |
| `Glob` | Find files by pattern (`**/*.rs`) |
| `Bash` | Execute shell commands |
| `WebFetch` | Fetch & strip HTML from URLs |
| `TodoRead` | Read task list |
| `TodoWrite` | Create/update tasks |
| `MemoryRead` | Read project & global memory |
| `MemoryWrite` | Save insights to memory |
| `InvokeAgent` | Delegate to a sub-agent |
| `ListAgents` | List available sub-agents |
| `CreateAgent` | Create a new sub-agent |
| `CreateTool` | Define a new custom tool |
| `ListTools` | List custom tools |
| `DeleteTool` | Remove a custom tool |

## REPL Commands

| Command | Description |
|---------|-------------|
| `/agent` | List available sub-agents |
| `/compact` | Summarize conversation to reclaim context |
| `/copy` | Copy last response or code block to clipboard |
| `/cost` | Show token usage for this session |
| `/diff` | Show uncommitted git changes |
| `/diff review` | Ask Koda to review uncommitted changes |
| `/diff commit` | Generate a commit message |
| `/help` | Command palette (select & execute) |
| `/memory` | View/save project & global memory |
| `/model` | Pick a model (↑↓ arrow keys) |
| `/paste` | Show clipboard contents |
| `/provider` | Pick a provider |
| `/sessions` | List recent sessions |
| `/quit` | Exit |

## Smart Input

```
🐻 [gpt-4o] ~/repo ❯ /mod         ← tab-completes to /model
🐻 [gpt-4o] ~/repo ❯ /model gpt   ← ghost hint: -4o-mini
🐻 [gpt-4o] ~/repo ❯ @src/main.rs ← injects file into context
🐻 [gpt-4o] ~/repo ❯ @screenshot.png ← sends image to multi-modal LLM
```

Drag-and-drop images from your file manager — Koda auto-detects
absolute paths to image files (png, jpg, gif, webp, bmp).

## Pre-built Sub-Agents

Koda ships with 4 specialized agents in `agents/`:

| Agent | Purpose | Tools |
|-------|---------|-------|
| `reviewer` | Critical code review | Read-only |
| `security` | Security audit (OWASP, CWEs) | Read + Bash |
| `testgen` | Find test gaps, write tests | Full access |
| `releaser` | GitHub release workflow | Full access |

Invoke them directly or let Koda delegate automatically:

```
🐻 Do a pre-release check

  🐻 Running 3 tools in parallel...

  ● InvokeAgent reviewer → 🔴 2 bugs, 🟡 3 warnings, 🟢 code quality good
  ● InvokeAgent security → 🟢 no critical issues, 🟡 1 medium
  ● InvokeAgent testgen  → wrote 8 new tests, all passing
```

Create your own agents by adding JSON files to `agents/`.
Use `/agent` to see what's available.

## Context Window Management

Koda manages context automatically:

- **Auto-compact** — when context reaches 80%, Koda summarizes the
  conversation via the LLM and replaces history with the summary
- **Manual compact** — `/compact` to summarize on demand
- **Sliding window** — old messages are dropped as a safety net
- **Context indicator** — prompt shows `(⚠ 82% context)` when usage is high

## Headless Mode

Run Koda as a one-shot CLI tool for CI/CD and scripting:

```bash
# Flag-based
koda -p "fix the login bug in src/auth.rs"

# Positional
koda "run tests and fix failures"

# Piped stdin (auto-detected)
echo "explain this error" | koda

# Explicit stdin
koda -p -

# JSON output for CI/CD
koda -p "list all TODOs" --output-format json
```

Headless mode skips the banner, onboarding, and version check.
Tools still work. Exit code 0 for success, 1 for errors.

## Memory System

Koda maintains persistent memory across sessions:

- **Project memory** — `MEMORY.md` in the project root
  (also reads `CLAUDE.md`, `AGENTS.md` for compatibility)
- **Global memory** — `~/.config/koda/memory.md`
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
| `MEMORY.md` | Project memory |
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
koda [OPTIONS] [PROMPT]

Arguments:
  [PROMPT]                     Run a single prompt and exit

Options:
  -p, --prompt <PROMPT>        Run a single prompt and exit (use "-" for stdin)
      --output-format <FMT>    Output format: text or json [default: text]
  -a, --agent <AGENT>          Agent to use [default: default]
  -s, --session <SESSION>      Session ID to resume
      --project-root <PATH>    Project root [default: .]
      --base-url <URL>         LLM provider base URL
      --model <MODEL>          Model name
      --provider <PROVIDER>    Provider name
  -h, --help                   Print help
  -V, --version                Print version
```

## Architecture

See [DESIGN.md](DESIGN.md) for the full architecture document.
See [FUTURE.md](FUTURE.md) for the roadmap and competitive analysis.

## Development

```bash
cargo test          # 288 tests across 6 suites
cargo build         # Debug build
cargo run           # Run locally
```

## License

MIT
