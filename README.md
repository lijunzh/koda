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
# macOS / Linux (Homebrew)
brew tap lijunzh/koda
brew install koda

# Cargo (requires Rust 1.99.1 or newer)
cargo install koda-cli

# From source
git clone https://github.com/lijunzh/koda.git
cd koda && cargo build --release -p koda-cli
```

### Run

On first run, an onboarding wizard will guide you through provider and API key setup.

```bash
# Open the interactive fullscreen TUI
koda

# One-shot prompt (headless mode for scripts/CI)
koda "fix the failing test in auth.rs"

# Specific model (an exact model ID; aliases like `claude-opus` are for /model in the TUI)
koda -p "explain auth.rs" --model claude-opus-5-5

# Pipe input directly into Koda
echo "review this diff" | koda
```

Inside the TUI, type `/help` to see all commands and keyboard shortcuts.

Koda no longer has an ACP server, so `koda server` and editor connections over
ACP are gone. [What changed, and what to use instead](https://lijunzh.github.io/koda/acp.html).

---

## Workspace Crates

| Crate | Description |
|---|---|
| [**koda-cli**](koda-cli/) | Terminal frontend (TUI + headless) |
| [**koda-core**](koda-core/) | Engine library — providers, tools, inference loop |
| [**koda-sandbox**](koda-sandbox/) | Capability-aware sandbox layer — kernel-enforced FS/net/exec policies |
| [**koda-test-utils**](koda-test-utils/) | Test utilities — mock providers, test sinks, and E2E harness |

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
