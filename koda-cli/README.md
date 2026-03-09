# koda-cli

CLI frontend for the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Built with [ratatui](https://ratatui.rs/) for an inline TUI experience —
streaming markdown, tab completion, diff previews, and approval widgets
without ever leaving the terminal.

## Install

```bash
cargo install koda-cli
```

## Quick start

```bash
koda                              # Interactive REPL
koda --provider anthropic         # Use a cloud provider
koda -p "fix the bug in auth.rs"  # Headless one-shot
koda server --stdio               # ACP server for editor integration
```

## Approval modes

Cycle with `Shift+Tab`:

| Mode | Behavior |
|------|----------|
| **Auto** | Phase-gated: writes confirmed before plan, auto-approved after |
| **Strict** | Every non-read action requires confirmation |
| **Safe** | Read-only: safe bash allowed, mutations blocked |

See the [README](https://github.com/lijunzh/koda) for full documentation.

## License

MIT
