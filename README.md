# Koda 🐻

[![CI](https://github.com/lijunzh/koda/actions/workflows/ci.yml/badge.svg)](https://github.com/lijunzh/koda/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/koda-cli.svg)](https://crates.io/crates/koda-cli)
[![Docs](https://img.shields.io/badge/docs-lijunzh.github.io%2Fkoda-blue)](https://lijunzh.github.io/koda/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A high-performance, terminal-native AI coding assistant built in Rust.**

*Single compiled binary. 15 LLM providers. Zero runtime dependencies. Your data stays local.*

---

## Quick Start

### Install

```bash
# macOS on Apple Silicon / Linux (Homebrew)
brew tap lijunzh/koda
brew install koda

# Cargo (requires Rust 1.100.0 or newer; the way to install on an Intel Mac)
cargo install koda-cli --locked

# From source
git clone https://github.com/lijunzh/koda.git
cd koda && cargo build --release -p koda-cli
```

### Run

On first run, an onboarding wizard will guide you through provider and API key setup, unless the command line already chose one (`--provider`, `--base-url`, or a model alias such as `--model claude-opus`).

```bash
# Open the interactive fullscreen TUI
koda

# One-shot prompt (headless mode for scripts/CI)
koda exec "fix the failing test in auth.rs"

# Specific model (an exact model ID; aliases like `claude-opus` are for /model in the TUI)
koda exec --model claude-opus-5-5 "explain auth.rs"

# Pipe input directly into Koda
echo "review this diff" | koda exec
```

Inside the TUI, type `/help` to see all commands and keyboard shortcuts.

Koda reads a project's `AGENTS.md`, or its `CLAUDE.md` if it has none, into
every request. What Koda saves for itself stays out of the project. Keep it short, especially with a small or local model. In Koda's
evals, a 24,000-character `CLAUDE.md` cost Haiku about half its passes on one
task, while Sonnet passed every run. See [what belongs in it](https://lijunzh.github.io/koda/memory.html).

Koda no longer has an ACP server, so `koda server` and editor connections over
ACP are gone. [What changed, and what to use instead](https://lijunzh.github.io/koda/acp.html).
A program can drive Koda with `koda serve --stdio`, which serves Koda's client
protocol on stdin and stdout ([Koda as a server](https://lijunzh.github.io/koda/serve.html)).

---

## Workspace Crates

| Crate | Description |
|---|---|
| [**koda-cli**](koda-cli/) | Terminal frontend (TUI + headless) |
| [**koda-core**](koda-core/) | Engine library — tools, inference loop |
| [**koda-providers**](koda-providers/) | Model providers (Anthropic, Gemini, OpenAI-compatible) and the model tables |
| [**koda-protocol**](koda-protocol/) | The client protocol: JSON-RPC 2.0 messages and their types |
| [**koda-client**](koda-client/) | The client API: Koda in-process, typed requests and the event queue |
| [**koda-server**](koda-server/) | Koda's host: sessions, config, MCP, and engine events projected onto the protocol; `koda serve` |
| [**koda-config**](koda-config/) | The config file, `~/.config/koda/config.toml`: reading, checking and editing it |
| [**koda-command**](koda-command/) | Shell command classification: what a command does, and the paths it touches |
| [**koda-mcp**](koda-mcp/) | The MCP client: connecting to MCP servers over stdio and HTTP, and bridging their tools |
| [**koda-skills**](koda-skills/) | Skills: `SKILL.md` files from the project, your config folder and the built-in set |
| [**koda-memory**](koda-memory/) | Memory: the instruction files people write (`AGENTS.md`, `CLAUDE.md`) and the notes Koda keeps |
| [**koda-state**](koda-state/) | Stored state — sessions (the `SessionStore` trait and its SQLite `Database`), Koda's SQLite files and the diagnostic log database |
| [**koda-sandbox**](koda-sandbox/) | Capability-aware sandbox layer — kernel-enforced FS/net/exec policies |
| [**koda-test-utils**](koda-test-utils/) | Test utilities — a stub HTTP server, mock providers, test sinks, and E2E harness |

## Documentation

| Resource | Description |
|---|---|
| [**User Manual**](https://lijunzh.github.io/koda/) | CLI reference, slash commands, trust modes, and custom agents |
| [**Engine API**](https://docs.rs/koda-core) | Developer docs for embedding `koda-core` |
| [**Design**](DESIGN.md) | Architecture principles and philosophies |
| [**Contributing**](CONTRIBUTING.md) | Workspace layout, coding conventions, and tests |
| [**Debugging**](https://lijunzh.github.io/koda/debugging.html) | Triage a run from its logs and reproduce a bug without an API key |
| [**Changelog**](CHANGELOG.md) | Version history |

## License

MIT
