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
  `@file` paths, a skill picker on `$` (see [Skills](./skills.md#loading-a-skill-yourself)),
  and reverse-search (`Ctrl+R`).
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

**Slash commands (`/compact`, `/model`, …) are not accepted
mid-inference,** except `/stop` and `/side` (below). Type `/compact` while the model is
thinking and you'll see a transient "slash commands disabled during
inference — press `Esc` first" hint instead of the text being silently
steered to the model. Press `Esc` to interrupt, then run the slash.
`/stop`, which stops background processes, runs mid-turn, as in Codex.

Press **`Tab`** instead to queue the text for after the turn, as Codex's `Tab`
does. It waits, and goes out as the next prompt once the turn completes, in a
turn of its own:

```
  ⏭ Queued: now update the docs too
```

Several queued messages go out one per turn, oldest first. A slash command
queues too and runs once the turn is over. `Alt+Up`, with the input box empty,
takes the message queued last back into it, to change it or send it at once
with `Enter`. A turn that is stopped or fails sends nothing queued: the queued
messages come back to the input box with any steering text the model didn't
get. To stop first, press `Esc`.

## Side conversations

`/side` (or `/btw`) asks a question about the work without adding it to the
session, as Codex's `/side` does. Koda forks the session into a side
conversation and switches to it:

```
/side which file held the retry settings?
```

- **It knows the session:** the side conversation has the whole history,
  and tools, so it can look things up.
- **It leaves the work alone:** it is told, in Codex's words, that the history
  is for reference only, and to change nothing (files, git, settings) unless
  you ask it to there. It starts no sub-agents.
- **The main session goes on.** `/side` works while a turn runs: the turn
  keeps going on its own. If it asks you something (an approval, a question),
  Koda says so, and the request waits for you.
- **`Ctrl+C` on an empty prompt goes back.** The side conversation is deleted,
  the main session is drawn again with what it did meanwhile, and a turn still
  running there shows as it goes on, with any request it made.

One side conversation at a time. Its rules reach the model through the
session's [`developer_instructions`](./configuration.md), which replace any you
set in `config.toml` for the side conversation only.

## Approval prompt

In `safe` mode, Koda asks before executing tools that change anything. In
`auto` mode, it still asks before destructive ones.
The menu area shows the tool name, the action detail, and hotkeys:

```
  Bash  rm -rf dist/
  [y] approve   [n] reject   [f] feedback   [a] auto   [esc] stop
```

For a local change inside the project, `[s] this session` also appears: yes,
and don't ask again this session for the same command or file.

See [Approval & trust modes](./approval.md) for the full reference.

## Notifications

When a turn ends, a tool call waits for approval or the model asks a
question, Koda tells you, as Codex does, while the terminal isn't focused:

- **Ghostty, iTerm2, Kitty, Warp and WezTerm** get a desktop notification
  (OSC 9) with the start of the reply, the command waiting, or the question.
  iTerm2 shows it once its alert settings are on (Settings → Profiles →
  Terminal → Notification Center Alerts).
- **Other terminals**, Apple Terminal among them, get the bell. Turn on
  "Audible bell" or "Visual bell" in Terminal's profile to hear or see it.

Koda knows the terminal isn't focused from the focus reports the terminal
sends when you switch away and back. Until the first one arrives it counts the
terminal as focused, so a terminal that never reports focus never notifies;
set `notification_condition = "always"` there. To check yours, run this,
switch to another window and back, then press `Ctrl+D`; `^[[O` and `^[[I`
mean it reports focus:

```sh
printf '\e[?1004h'; command cat -v; printf '\e[?1004l'
```

(`command` skips a `cat` alias such as `bat`; the last `printf` turns the
reports off again.)

**Inside tmux,** add `set -g focus-events on` to `~/.tmux.conf` so tmux
passes focus reports on. The bell needs nothing more; a desktop notification
also needs `set -g allow-passthrough on`.

Settings, in `[tui]` of `config.toml`:

```toml
[tui]
notifications = true            # or false, or a list of events
notification_method = "auto"    # or "osc9", "osc777", "bel"
notification_condition = "unfocused"  # or "always"
```

The events are `agent-turn-complete`, `approval-requested` and
`user-input-requested`.

See also [Slash commands](./commands.md) and [Keybindings](./keybindings.md).
