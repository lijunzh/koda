# Koda 🐻

A high-performance AI coding agent built in Rust.

Single compiled binary. Multi-provider LLM support. Zero runtime dependencies.

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
koda                              # Interactive REPL (auto-detects LM Studio)
koda --provider anthropic         # Use a cloud provider
koda -p "fix the bug in auth.rs"  # Headless one-shot
echo "explain this" | koda        # Piped input
```

Type `/help` in the REPL for all commands and shortcuts.

## Highlights

- **20+ built-in tools** — file ops, search, shell, web fetch, memory, agents, AST analysis, email
- **14 LLM providers** — LM Studio, OpenAI, Anthropic, Gemini, Groq, Grok, Ollama, DeepSeek, and more
- **User-defined agents** — specialized sub-agents via JSON configs with per-agent model selection
- **Safety** — git checkpointing, approval modes, per-tool safety gates, folder-scoped permissions
- **Fullscreen TUI** — mouse scroll, clipboard copy, diff preview, extended thinking display
- **Headless mode** — `koda -p "prompt"` with JSON output for CI/CD
- **Skills** — built-in expertise modules (code review, security audit) + user-created skills

## Architecture

```
koda/
├── koda-core/     # Engine library (providers, tools, inference, DB)
├── koda-cli/      # CLI binary (REPL, display, approval UI)
├── koda-ast/      # Tree-sitter AST analysis (library + MCP server)
└── koda-email/    # Email via IMAP/SMTP (library + MCP server)
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

See [docs.rs/koda-core](https://docs.rs/koda-core) for full config reference.

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
