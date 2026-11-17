# Koda 🐻

[![CI](https://github.com/lijunzh/koda/actions/workflows/ci.yml/badge.svg)](https://github.com/lijunzh/koda/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/koda-cli.svg)](https://crates.io/crates/koda-cli)
[![Docs](https://img.shields.io/badge/docs-lijunzh.github.io%2Fkoda-blue)](https://lijunzh.github.io/koda/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/lijunzh/koda/blob/main/LICENSE)

**Koda is an AI coding agent for the terminal, on macOS and Linux.** It reads
and edits your project, runs commands in a kernel sandbox, and works with
Claude, OpenAI-compatible servers and local models. One Rust binary.

## Install

```bash
# Homebrew (Apple Silicon Macs and Linux)
brew tap lijunzh/koda
brew install koda

# Cargo (Rust 1.100.0 or newer; also how to install on an Intel Mac)
cargo install koda-cli --locked
```

On Linux, Koda's sandbox needs bubblewrap: `apt install bubblewrap` (or your
distribution's package). Reading PDFs uses poppler, if you want it: `brew
install poppler`, or `apt install poppler-utils`.
[Installation](https://lijunzh.github.io/koda/installation.html) has the
details and building from source.

## Run

```bash
koda                                   # the full-screen TUI
koda "why do the auth tests fail?"     # the TUI, starting with a prompt
koda exec "fix the failing test"       # one prompt without the TUI, for scripts and CI
git diff | koda exec "review this"     # piped input
koda resume --last                     # pick up this project's latest session
koda --oss                             # a local model, no API key (needs llama.cpp)
```

On first run Koda opens its provider picker: choose a provider and give it an
API key, or point it at a local server. Or try it free with `brew install
llama.cpp` and `koda --oss`: Koda starts a local model for the session and
stops it when you quit (Qwen3.8-Flash-Next on a 128 GB Mac, else a 1 GB quick
try). See [Local models](https://lijunzh.github.io/koda/local-models.html). Inside the TUI, `/help` lists the
commands and keys.

## What it does

- **Tools for a codebase:** read (text, images, PDFs, notebooks), search,
  edit, shell commands, the web, sub-agents, a todo list and memory.
- **Trust modes:** Auto (the default) runs changes inside the sandbox and asks
  before destructive or outside-project actions; Safe asks before every
  change; Plan only looks. `Shift+Tab` cycles them.
- **A kernel sandbox for every command:** Seatbelt on macOS, bubblewrap on
  Linux. Writes stay in the project and temporary folders, credential folders
  are blocked, and the network goes through Koda's proxy.
- **15 providers:** Anthropic, OpenAI, Gemini, OpenRouter, Groq, xAI, DeepSeek,
  Mistral, and local servers (LM Studio, Ollama, llama.cpp, vLLM) among them.
  `koda --oss` runs a local model with nothing to set up.
- **Your project's instructions:** `AGENTS.md`, or `CLAUDE.md` if there is
  none; [skills](https://lijunzh.github.io/koda/skills.html), [custom
  agents](https://lijunzh.github.io/koda/agents.html) and [MCP
  servers](https://lijunzh.github.io/koda/mcp.html).
- **Sessions** saved per project, to resume or fork.
- **For programs:** `koda exec --json` streams the run as JSON-RPC lines, and
  `koda serve --stdio` serves Koda's [client
  protocol](https://lijunzh.github.io/koda/serve.html) to a GUI, an editor or a
  script.

**Where your data goes:** Koda keeps its sessions and settings on your
machine, in `~/.config/koda`. What you send the model, including the files it
reads, goes to the provider you choose; with a local server, the conversation
stays on your machine. Web and MCP tools reach the sites and servers they
name, and Koda asks crates.io for new versions. No telemetry. See [Privacy &
data](https://lijunzh.github.io/koda/privacy.html).

## Documentation

- [**User manual**](https://lijunzh.github.io/koda/): installation, the TUI,
  trust modes, the sandbox, configuration and every command.
- [**Upgrading to 0.7**](https://lijunzh.github.io/koda/upgrading-0.7.html):
  what changed and what was removed.
- [**Changelog**](https://github.com/lijunzh/koda/blob/main/CHANGELOG.md)
- [**Design**](https://github.com/lijunzh/koda/blob/main/DESIGN.md) and
  [**contributing**](https://github.com/lijunzh/koda/blob/main/CONTRIBUTING.md),
  for working on Koda.

## Crates

`koda-cli` is the program. The others are its parts, published so it can be;
their APIs aren't stable before 1.0.

| Crate | What it is |
|---|---|
| [koda-cli](https://crates.io/crates/koda-cli) | The `koda` program: the TUI, `koda exec` and `koda serve` |
| [koda-client](https://crates.io/crates/koda-client) | The client API: Koda in-process, typed requests and the event queue. Start here to embed Koda |
| [koda-protocol](https://crates.io/crates/koda-protocol) | The client protocol: JSON-RPC 2.0 messages and their types |
| [koda-server](https://crates.io/crates/koda-server) | Koda's host: sessions, settings and MCP, behind the protocol |
| [koda-core](https://crates.io/crates/koda-core) | The engine: the agent loop, tools, approval and compaction |
| [koda-providers](https://crates.io/crates/koda-providers) | Model providers and the model tables |
| [koda-config](https://crates.io/crates/koda-config) | `~/.config/koda/config.toml`: reading, checking and editing it |
| [koda-state](https://crates.io/crates/koda-state) | Stored state: sessions, SQLite files and the diagnostic log database |
| [koda-sandbox](https://crates.io/crates/koda-sandbox) | The kernel sandbox: Seatbelt on macOS, bubblewrap on Linux |
| [koda-command](https://crates.io/crates/koda-command) | Shell command classification: what a command does and the paths it touches |
| [koda-mcp](https://crates.io/crates/koda-mcp) | The MCP client, over stdio and HTTP |
| [koda-skills](https://crates.io/crates/koda-skills) | Skills: `SKILL.md` files from the project, your config folder and the built-in set |
| [koda-memory](https://crates.io/crates/koda-memory) | Instruction files (`AGENTS.md`, `CLAUDE.md`) and the notes Koda keeps |

## License

MIT
