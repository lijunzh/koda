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
│  claude-sonnet-4 │ auto │ ████████░░ 78% │ ⏳ 8s                        │
└──────────────────────────────────────────────────────────────────────────┘
```

## Layout

- **History panel** — scrollable conversation history. Supports
  syntax-highlighted code blocks, rendered markdown, and collapsible
  tool-call summaries.
- **Input** — multi-line editor with tab-completion for slash commands and
  `@file` paths, and reverse-search (`Ctrl+R`).
- **Status bar** — live view of model, trust mode, current working
  directory (right-truncated to fit, with `~` substitution for `$HOME`),
  context usage bar, MCP server count (when configured), inference time,
  and last-turn token/speed stats.

## Starting a conversation

Type your message and press `Enter`. Koda streams the response in real time.

## Typing during inference

You can type while Koda is thinking or running tools. The input area stays
active throughout.

Press **`Enter`** during inference. Your text is sent directly to the engine
and injected into the **current turn** before the next provider request. The
model sees it immediately — useful for course-correcting mid-task.

```
  📥 Next: add the missing error handling too
```

If the model finishes its reply first, the turn goes on so it can answer you.
Text sent while an approval or a question is open waits for the tool to finish.

If the turn ends without delivering your text, because you pressed `Esc`, chose
to stop at the iteration limit, or the turn failed, the text returns to the
input box. Press `Enter` to send it as a new prompt.

**Slash commands (`/cancel`, `/compact`, `/model`, …) are not accepted
mid-inference** (#1211/#1222). Type `/cancel agent:1` while the model is
thinking and you'll see a transient "slash commands disabled during
inference — press `Esc` first" hint instead of the text being silently
steered to the model. Press `Esc` to interrupt, then run the slash.

To add work for after the current task, say so: "when you're done, update the
docs". To stop first, press `Esc`.

## Approval prompt

In `safe` mode, Koda asks before executing tools that change anything. In
`auto` mode, it still asks before destructive ones.
The menu area shows the tool name, the action detail, and hotkeys:

```
  Bash  rm -rf dist/
  [y] approve   [n] reject   [f] feedback   [a] always   [esc] stop
```

See [Approval & trust modes](./approval.md) for the full reference.

See also [Slash commands](./commands.md) and [Keybindings](./keybindings.md).
