# Keybindings

## Input

| Key | Action |
|-----|--------|
| `Enter` | Send message (or queue as **next** during inference; slash commands rejected mid-inference — press `Esc` first) |
| `Ctrl+J` | Queue message as **later** during inference (slash commands rejected mid-inference — press `Esc` first) |
| `Alt+Enter` | Insert newline (multi-line input) |
| `Tab` | Autocomplete slash commands and `@file` paths |
| `Shift+Tab` | Cycle trust mode (Safe ↔ Auto) |
| `↑ / ↓` | Cycle through input history (idle) · pop `later` queue into editor (during inference) |
| `Ctrl+R` | Reverse history search |
| `Ctrl+U` | Clear deferred (`later`) queue during inference |

## Navigation

| Key | Action |
|-----|--------|
| `PgUp / PgDn` | Scroll history one page up / down |
| `Home` | Jump to top of history |
| `End` | Jump to bottom (latest output) |
| Mouse scroll | Scroll conversation history |

## Session control

| Key | Action |
|-----|--------|
| `Esc` | Cancel current inference |
| `Ctrl+C` | Cancel current inference |
| `Ctrl+D` | Quit koda |

## Editor (composer)

Deletion chords inside the input composer. The redundant variants exist
so users coming from different terminals / OSes hit a working binding
on the first try (ported from upstream codex in #1278 to match codex's
default keymap).

| Key | Action |
|-----|--------|
| `Backspace` &middot; `Shift+Backspace` &middot; `Ctrl+H` | Delete character before cursor |
| `Delete` &middot; `Shift+Delete` &middot; `Ctrl+D`¹ | Delete character after cursor |
| `Alt+Backspace` &middot; `Ctrl+Backspace` &middot; `Ctrl+Shift+Backspace` &middot; `Ctrl+W` | Delete word before cursor |
| `Alt+Delete` &middot; `Ctrl+Delete` &middot; `Ctrl+Shift+Delete` | Delete word after cursor |

¹ `Ctrl+D` deletes a character only when the input has content; on an
empty input it quits koda (see *Session control* above).

## Approval prompt

These keys appear when the agent asks to execute a tool:

| Key | Action |
|-----|--------|
| `y` | Approve this action |
| `n` | Reject this action |
| `a` | Approve and switch to auto mode (no more confirmations this session) |
| `f` | Reject and type written feedback explaining why |
| `Esc` | Reject (same as `n`) |

## Vim mode

Toggle with [`/vim`](./commands.md#vim). Once enabled, the input
composer behaves like a single-buffer vi.

**Modes:** Insert (default after toggle) ↔ Normal (`Esc`).

| Mode | Keys | Action |
|------|------|--------|
| Normal | `i`, `a`, `o`, `O`, `I`, `A` | Re-enter Insert at various positions |
| Normal | `h j k l` | Move cursor left/down/up/right |
| Normal | `w`, `b`, `e` | Word forward / back / end-of-word |
| Normal | `0`, `^`, `$` | Beginning of line / first non-blank / end of line |
| Normal | `gg`, `G` | Jump to first / last line |
| Normal | `x`, `dd`, `yy`, `p`, `P` | Delete char / line · yank line · paste after / before |
| Normal | `cc`, `ci<delim>`, `ca<delim>` | Change line / inside / around delimiter |
| Normal | `dw`, `db`, `de`, `d$`, `d0` | Delete by motion |
| Normal | `u`, `Ctrl+R` | Undo · Redo |
| Normal | `:` | (Reserved — no commands wired yet) |

**Caveat:** Vim mode is per-session and the slash command toggles it
on/off; it does not persist across koda restarts. The setting also does
not affect Approval-prompt keys above (those remain `y`/`n`/`a`/`f`/`Esc`).

## Composer key hints

A single-line footer below the input shows context-sensitive key hints
(e.g. `Enter send · Alt+Enter newline · Tab complete`). The hints update
based on whether the input is empty, has content, has an active dropdown,
or is in vim mode. Added in v0.2.27 (#1183).
