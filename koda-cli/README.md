# koda-cli

CLI frontend for the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Built with [ratatui](https://ratatui.rs/) for an inline TUI experience —
streaming markdown, tab completion, diff previews, and approval widgets
without ever leaving the terminal.

## Install

```bash
cargo install koda-cli
```

On first run, an onboarding wizard guides you through provider and API key setup.

## Quick start

```bash
koda                              # Interactive REPL
koda --provider anthropic         # Use a cloud provider
koda -p "fix the bug in auth.rs"  # Headless one-shot
koda server --stdio               # ACP server for editor integration
```

## Trust modes

Cycle with `Shift+Tab`:

| Mode | Behavior |
|------|----------|
| **Safe** (default) | Confirm every side effect. Read-only tools auto-approved. |
| **Auto** | Auto-approve all actions within the project sandbox. |

A third mode, **Plan**, is available for agent definitions that need
investigation-only access — all writes are denied.

```bash
# Start in Auto mode
koda --mode auto

# Via environment variable
export KODA_MODE=auto
```

## Sandbox

The kernel sandbox is **always active** — every Bash command runs inside
a sandboxed process with credential protection. No opt-out.

- **Writes** restricted to project dir + `/tmp` + cache dirs
- **Credential dirs** blocked: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, …
- **Agent files** protected: `.koda/agents/` and `.koda/skills/` are read-only

macOS uses `sandbox-exec` (seatbelt); Linux uses `bwrap` (bubblewrap).
Sub-agents inherit the parent's trust mode via `TrustMode::clamp()` and
can never run with less protection.

See the [User Manual](https://lijunzh.github.io/koda/) for full documentation.

## License

MIT
