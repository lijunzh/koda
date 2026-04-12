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

## Approval modes

Cycle with `Shift+Tab`:

| Mode | Behavior |
|------|----------|
| **Auto** | Local mutations auto-approved, destructive ops need confirmation |
| **Confirm** | Every non-read action requires confirmation |

## Sandbox

Opt-in process sandboxing restricts what the Bash tool can do:

```bash
# Restrict writes to project dir + /tmp
koda --sandbox project

# + block reads to credential dirs (~/.ssh, ~/.aws, ~/.gnupg, …)
koda --sandbox strict

# Via environment variable
export KODA_SANDBOX=strict
```

macOS uses `sandbox-exec` (seatbelt); Linux uses `bwrap` (bubblewrap).
Sub-agents inherit the parent’s sandbox mode and can never run weaker.
See the [sandbox reference](https://lijunzh.github.io/koda/sandbox.html)
for the full list of protected paths.

See the [User Manual](https://lijunzh.github.io/koda/) for full documentation.

## License

MIT
