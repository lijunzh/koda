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
│────────────────────────────────────────────────────── 🐻 ────────────────│
│  > _                                                                     │
│  📋 1  also update the README                                            │
│  📋 2  then run the linter                                               │
│  ↑ pop  ·  Ctrl+U clear                                                 │
│  claude-sonnet-4 │ auto │ ████████░░ 78% │ ⏳ 8s │ 📋 2 queued ^U clear │
└──────────────────────────────────────────────────────────────────────────┘
```

## Layout

- **History panel** — scrollable conversation history. Supports
  syntax-highlighted code blocks, rendered markdown, and collapsible
  tool-call summaries.
- **Input** — multi-line editor with tab-completion for slash commands and
  `@file` paths, and reverse-search (`Ctrl+R`).
- **Queue preview** — shown above the status bar **only when the deferred
  queue has items**. Displays up to 3 rows of pending text with index numbers,
  an overflow count, and keybinding hints. Hidden when the queue is empty.
- **Status bar** — live view of model, trust mode, context usage bar, MCP
  server count (when configured), inference time, queue count, and last-turn
  token/speed stats.

## Starting a conversation

Type your message and press `Enter`. Koda streams the response in real time.

## Typing during inference

You can type while Koda is thinking or running tools. The input area stays
active throughout. Two lanes are available:

### Next lane — steer the current turn (default)

Press **`Enter`** during inference. Your text is sent directly to the engine
and injected into the **current turn** before the next provider request. The
model sees it immediately — useful for course-correcting mid-task.

```
  📥 Next: add the missing error handling too
```

### Later lane — queue for the next turn

Press **`Ctrl+J`** during inference. Your text is deferred to the `later`
queue and will run as a **single new turn** after the current one completes.
All deferred items are joined and submitted together, so the model sees them
as one message.

```
  📋 Later (1): also bump the version number
```

### Queue preview widget

While items are in the `later` queue, a preview panel appears above the
status bar:

```
  📋 1  also bump the version number
  📋 2  and update the changelog
  ↑ pop  ·  Ctrl+U clear
```

- **`↑`** (Up Arrow) — pops the last item back into the editor so you can
  edit it before re-submitting.
- **`Ctrl+U`** — clears all deferred items without cancelling inference.
- **`Esc` / `Ctrl+C`** — cancels inference and also clears the queue so
  deferred messages don't fire unexpectedly after a cancelled turn.

### Choosing a lane

| Situation | Use |
|---|---|
| "Actually, also handle the edge case" | `Enter` (next — stays in this turn) |
| "After this is done, update the docs" | `Ctrl+J` (later — new turn when done) |
| Changed your mind about a later item | `↑` to pop it back into the editor |
| Abort everything | `Esc` then clear queue with `Ctrl+U` if needed |

## Approval prompt

When trust mode is `safe` or `plan`, Koda asks before executing tools.
The menu area shows the tool name, the action detail, and hotkeys:

```
  Bash  rm -rf dist/
  [y] approve   [n] reject   [f] feedback   [a] always
```

See [Approval & trust modes](./approval.md) for the full reference.

See also [Slash commands](./commands.md) and [Keybindings](./keybindings.md).
