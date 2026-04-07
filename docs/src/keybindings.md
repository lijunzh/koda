# Keybindings

## Input

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Alt+Enter` | Insert newline (multi-line input) |
| `Tab` | Autocomplete slash commands and `@file` paths |
| `Shift+Tab` | Toggle approval mode (auto ↔ confirm) |
| `↑ / ↓` | Cycle through input history |
| `Ctrl+R` | Reverse history search |

## Navigation

| Key | Action |
|-----|--------|
| `PgUp / PgDn` | Scroll history one page up / down |
| `Home` | Jump to top of history |
| `End` | Jump to bottom (latest output) |
| Mouse scroll | Scroll conversation history |
| `Ctrl+Y` | Copy last code block to clipboard |
| `Ctrl+U` | Copy last assistant response to clipboard |

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
