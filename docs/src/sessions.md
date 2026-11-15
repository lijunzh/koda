# Session management

Koda stores every conversation in a local SQLite database, organised by
project root. Each session gets a UUID that you can use to resume it.

```bash
# List and pick a session interactively
/resume

# Resume by ID prefix from the TUI
/resume a3f8bc

# Resume from the command line, with the full ID: in the TUI, or headless
koda -s a3f8bc12-1587-4c6d-a855-46183ca84fba
koda exec -s a3f8bc12-1587-4c6d-a855-46183ca84fba "continue where we left off"

# Delete a session permanently
/resume delete a3f8bc
```

## What a resumed session shows

Its earlier turns, drawn as they were live, as Codex's TUI replays a
resumed thread: your prompts, the replies, tool calls folded as they were
(`Ctrl+O` unfolds them), a sub-agent's `Done (N tool uses · Ns)`, what
happened to a refused or blocked call, and how long each turn worked. A
`session resumed` line follows them, and a banner when the last turn was
interrupted; type `continue` to pick it up. `koda -s`, `/resume` and the
sessions picker all show them.

Sessions stored before 0.6.5 are rebuilt from their messages, so they show
less: no reasoning, and no time on a sub-agent's summary.

## Session title

In the TUI, the first 50 characters of your first message become the session's
title, shown in `/resume`.
