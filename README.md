# Koda 🐻

[![CI](https://github.com/lijunzh/koda/actions/workflows/ci.yml/badge.svg)](https://github.com/lijunzh/koda/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/koda-cli.svg)](https://crates.io/crates/koda-cli)
[![docs.rs](https://docs.rs/koda-core/badge.svg)](https://docs.rs/koda-core)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A high-performance personal AI assistant built in Rust.

Single compiled binary. 14 LLM providers. Zero runtime dependencies.

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
```

On first run, an onboarding wizard guides you through provider and API key setup.

## Quick Start

```bash
koda                                # Interactive REPL (auto-detects local models)
koda "fix the failing test"         # One-shot with positional prompt
koda -p "explain auth.rs" -m opus   # Explicit model alias
echo "review this diff" | koda      # Piped input
koda server --stdio                 # ACP server for editor integration
```

Model aliases route to the right provider automatically:

| Alias | Provider | Model |
|---|---|---|
| `flash-lite` | Gemini | gemini-2.0-flash-lite |
| `flash` | Gemini | gemini-2.5-flash |
| `pro` | Gemini | gemini-2.5-pro |
| `haiku` | Anthropic | claude-3-5-haiku |
| `sonnet` | Anthropic | claude-sonnet-4 |
| `opus` | Anthropic | claude-opus-4 |

Type `/help` in the REPL for all commands and shortcuts.

## Highlights

- **18 built-in tools** — file ops, search, shell, web fetch/search, memory, sub-agents, skills
- **14 LLM providers** — OpenAI, Anthropic, Gemini, Groq, Grok, Ollama, DeepSeek, LM Studio, and more
- **Model aliases** — `opus`, `sonnet`, `flash` route across providers without remembering model IDs
- **Sub-agents** — specialized agents via JSON configs with per-agent model/tool selection
- **Safety** — git checkpointing, approval modes, per-tool safety gates, folder-scoped permissions
- **Fullscreen TUI** — mouse scroll, clipboard copy, diff preview, extended thinking display
- **Headless mode** — `koda "prompt"` with JSON output for CI/CD pipelines
- **ACP server** — `koda server --stdio` for editor/IDE integration
- **Skills** — built-in expertise modules (code review, security audit) + user-created skills

## Architecture

```
koda/
├── koda-core/     # Engine library (providers, tools, inference, DB)
├── koda-cli/      # CLI binary (REPL, TUI, approval UI, ACP server)
├── koda-ast/      # Tree-sitter AST analysis library
└── koda-email/    # Email via IMAP/SMTP library
```

## Custom Agents

```json
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist.",
  "model": "gemini-2.5-flash",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

Place in `.koda/agents/` (project) or `~/.config/koda/agents/` (global).

## Documentation

| Document | Content |
|---|---|
| [**API Reference**](https://docs.rs/koda-core) | Auto-generated from code |
| [**Design**](DESIGN.md) | Architecture principles |
| [**CLAUDE.md**](CLAUDE.md) | Workspace layout, conventions |
| [**Changelog**](CHANGELOG.md) | Version history |

## Development

```bash
cargo test --workspace --features koda-core/test-support
cargo clippy --workspace
cargo run -p koda-cli
```

## License

MIT
