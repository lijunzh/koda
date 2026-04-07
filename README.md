# Koda 🐻

[![CI](https://github.com/lijunzh/koda/actions/workflows/ci.yml/badge.svg)](https://github.com/lijunzh/koda/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/koda-cli.svg)](https://crates.io/crates/koda-cli)
[![Docs](https://img.shields.io/badge/docs-lijunzh.github.io%2Fkoda-blue)](https://lijunzh.github.io/koda/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A high-performance, terminal-native AI coding assistant built in Rust.**

*Single compiled binary. 14 LLM providers. Zero runtime dependencies. Your data stays local.*

---

## 🚀 Quick Start

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

Inside the TUI, type `/help` to see all commands and keyboard shortcuts!

---

## 📚 Documentation

The complete **[Koda User Manual](https://lijunzh.github.io/koda/)** has everything you need:

| Resource | Description |
|---|---|
| 📖 **[User Manual](https://lijunzh.github.io/koda/)** | CLI reference, slash commands, file attachments, approval modes, and custom agents. |
| ⚙️ **[Engine API](https://docs.rs/koda-core)** | Developer docs for embedding the `koda-core` library. |
| 🏗️ **[Design](DESIGN.md)** | Core architecture principles and philosophies. |
| 🛠️ **[Contributing](CLAUDE.md)** | Workspace layout, coding conventions, and tests. |
| 📜 **[Changelog](CHANGELOG.md)** | Version history. |

---

## ✨ Highlights

- **Local-First & Private:** Your conversations and API keys never leave your machine (stored in a local SQLite DB). Zero telemetry.
- **14 LLM Providers:** Support for Anthropic, OpenAI, Gemini, DeepSeek, Groq, local models (Ollama, LM Studio), and more.
- **Smart Model Aliases:** Use shortcuts like `--model sonnet` or `/model flash` to instantly route to the right provider. No need to memorize exact model IDs.
- **18 Built-in Tools:** File operations, codebase search (`rg`), shell execution, web fetching, memory management, and dynamic sub-agents.
- **Safe Execution:** Read-only tasks run automatically. Destructive actions prompt for your explicit approval (`Auto` vs `Confirm` modes).
- **Rich TUI:** Fullscreen interface with syntax highlighting, mouse scrolling, diff previews, and a collapsible view for agent tool execution.
- **Editor Integration:** Built-in [Agent Client Protocol (ACP)](https://agentclientprotocol.org) server for seamless use inside VS Code, Zed, etc.
- **Skills:** Built-in expertise modules (code review, security audit) + support for user-created `.md` skills.

---

## 🏗️ Architecture

Koda is split into modular, reusable crates:

```text
koda/
├── koda-cli/      # The CLI app (TUI, headless dispatch, ACP server)
├── koda-core/     # The core engine (providers, inference loop, DB, tools)
├── koda-ast/      # Tree-sitter AST analysis library
└── koda-email/    # IMAP/SMTP email library
```

---

## 🛠️ Development

```bash
# Run the full test suite
cargo test --workspace --features koda-core/test-support

# Run lints
cargo clippy --workspace

# Run the CLI locally
cargo run -p koda-cli
```

## License

MIT
