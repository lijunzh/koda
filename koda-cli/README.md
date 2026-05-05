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

Linux note: the default trust mode is Auto, which requires bubblewrap
(`bwrap`) so the kernel sandbox can contain auto-approved mutations.
Install it with your package manager (`apt install bubblewrap`,
`dnf install bubblewrap`, `pacman -S bubblewrap`) or start with
`koda --mode safe` to keep the human in every approval loop.

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
| **Auto** (default) | Auto-approve safe in-project mutations within the kernel sandbox. Destructive ops and outside-project writes still ask. Requires sandbox availability. |
| **Safe** | Confirm every mutation. Read-only tools auto-approved. Use this for CI or locked-down machines. |

A third mode, **Plan**, is available for agent definitions that need
investigation-only access — all writes are denied.

```bash
# Keep the old prompt-every-mutation behavior
koda --mode safe

# Via environment variable
export KODA_MODE=safe
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
