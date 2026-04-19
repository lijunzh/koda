# Koda 🐻

[![CI](https://github.com/lijunzh/koda/actions/workflows/ci.yml/badge.svg)](https://github.com/lijunzh/koda/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/koda-cli.svg)](https://crates.io/crates/koda-cli)
[![Docs](https://img.shields.io/badge/docs-lijunzh.github.io%2Fkoda-blue)](https://lijunzh.github.io/koda/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A high-performance, terminal-native AI coding assistant built in Rust.**

*Single compiled binary. 14 LLM providers. Zero runtime dependencies. Your data stays local.*

---

## Quick Start

### Install

```bash
# macOS / Linux (Homebrew)
brew tap lijunzh/koda
brew install koda

# Cargo (requires Rust toolchain)
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

# Explicit model alias
koda -p "explain auth.rs" -m opus

# Pipe input directly into Koda
echo "review this diff" | koda

# Start the ACP server for editor/IDE integration
koda server --stdio
```

Inside the TUI, type `/help` to see all commands and keyboard shortcuts.

---

## Workspace Crates

| Crate | Description |
|---|---|
| [**koda-cli**](koda-cli/) | Terminal frontend (TUI + headless + ACP server) |
| [**koda-core**](koda-core/) | Engine library — providers, tools, inference loop |

## Documentation

| Resource | Description |
|---|---|
| [**User Manual**](https://lijunzh.github.io/koda/) | CLI reference, slash commands, trust modes, and custom agents |
| [**Engine API**](https://docs.rs/koda-core) | Developer docs for embedding `koda-core` |
| [**Design**](DESIGN.md) | Architecture principles and philosophies |
| [**Contributing**](CLAUDE.md) | Workspace layout, coding conventions, and tests |
| [**Changelog**](CHANGELOG.md) | Version history |

## License

MIT
