# Approval modes

Koda has two approval modes, toggled with `Shift+Tab` (current mode shown
in the status bar):

**Auto** — safe tools run without confirmation. Destructive shell commands
(`rm -rf`, `sudo`, `git push --force`, etc.) still require explicit `y`.
Read-only tools (Read, Grep, Glob, WebFetch) are always auto-approved.

**Confirm** — every write or mutation requires explicit `y` before executing.
Read-only tools are still auto-approved.

The mode is **persisted per session** — if you approve with `a` (auto),
that session remembers it even after resuming.

In headless mode, there is no human to prompt. Destructive Bash commands
are silently rejected; all other tools proceed automatically.

## Approval keys

| Key | Effect |
|-----|--------|
| `y` | Approve this one action |
| `n` | Reject this one action |
| `a` | Approve and enable auto mode for the rest of the session |
| `f` | Reject and provide written feedback the model can act on |
| `Esc` | Reject (same as `n`) |
