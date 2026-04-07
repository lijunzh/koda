# Interactive TUI

Run `koda` with no arguments to open the full-screen TUI.

```text
┌──────────────────────────────────────────────────────────────────────────┐
│  [conversation history — scrollable with PgUp/PgDn]                     │
│                                                                          │
│  ⚡ Bash   cargo test                                                    │
│  │ running 42 tests …                                                   │
│  ✓ Bash (exit 0)                                                         │
│                                                                          │
│  All tests pass! Here's what I changed in `auth.rs` …                   │
├────────────── claude-sonnet · auto · 34% · 8s ───────────────────────────┤
│  > _                                                                     │
└──────────────────────────────────────────────────────────────────────────┘
```

The status bar shows: **model** · **approval mode** · **context %** · **elapsed**

## Layout

- **Top panel** — conversation history. Scrollable; supports syntax-highlighted
  code blocks, rendered markdown, and collapsible tool-call summaries.
- **Status bar** — live view of model, approval mode, context usage, and
  inference time.
- **Input** — multi-line editor with history, tab-completion for slash commands
  and `@file` paths, and reverse-search (`Ctrl+R`).

## Starting a conversation

Just type and press `Enter`. Koda streams the response in real time.
While inference is running, `Esc` or `Ctrl+C` cancel the current turn.

See [slash commands](./commands.md) for all in-session commands and
[keybindings](./keybindings.md) for the full keyboard reference.
