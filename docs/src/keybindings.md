# Keybindings

## Input

| Key | Action |
|-----|--------|
| `Enter` | Send message (or queue as **next** during inference) |
| `Ctrl+J` | Queue message as **later** during inference (deferred turn) |
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

## Approval prompt

These keys appear when the agent asks to execute a tool:

| Key | Action |
|-----|--------|
| `y` | Approve this action |
| `n` | Reject this action |
| `a` | Approve and switch to auto mode (no more confirmations this session) |
| `f` | Reject and type written feedback explaining why |
| `Esc` | Reject (same as `n`) |
