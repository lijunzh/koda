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
│──────────────────────────────────────────────────────────────────────────│
│  > _                                                                     │
│  claude-sonnet-4 │ auto │ ████████░░ 78% │ ⏳ 8s                        │
└──────────────────────────────────────────────────────────────────────────┘
```

When it opens, Koda prints its logo, a bear's paw print drawn in dots, beside
the version, the model with its provider, and the folder. The print needs a
font with Braille characters, which terminal emulators have; on the bare
Linux console the three lines show without it.

## Layout

- **History panel** — scrollable conversation history. Supports
  syntax-highlighted code blocks, rendered markdown, and folded steps
  (below).
- **Working area** — while a turn runs, right above the input: what Koda
  is doing now and for how long (`Thinking… 12s`, `Reading Cargo.toml… 2s`,
  `Running a command… 1m 2s`, `Waiting for your approval`), and under it
  the turn's plan, when the model keeps one, with each step done, in
  progress or to do. At most five steps show; `Ctrl+T` hides or shows the
  plan.
- **Input** — multi-line editor with tab-completion for slash commands and
  `@file` paths, a skill picker on `$` (see [Skills](./skills.md#loading-a-skill-yourself)),
  and reverse-search (`Ctrl+R`).
- **Status bar** — live view of model, trust mode, current working
  directory (right-truncated to fit, with `~` substitution for `$HOME`),
  context usage bar, MCP server count (when configured), inference time,
  and last-turn token/speed stats.

## Starting a conversation

Type your message and press `Enter`. Koda streams the response in real time.

## What a turn shows

The steps between your prompt and the answer are folded, as Claude Code
folds them, so the answer is easy to find:

```text
  ∴ Thought for 12s (ctrl+o to show thinking)
● Read 3 files, searched 2 patterns, ran 4 commands (ctrl+o to expand)
  ───
  The docs describe the GUI plan in two places …
● Bash cargo test
  │ running 42 tests …
```

- **Thinking** shows in the working area while it runs, then leaves one
  line, `∴ Thought for 12s`.
- **Lookups** made one after another (reads, searches, listings, and
  commands that only read) are one row with no output. A failed one is
  counted on the row in red. Text from Koda, or any other step, starts a
  new row.
- **Other commands** keep their own row and the first lines of what they
  printed, as Claude Code shows them: no header, the exit code only when the
  command failed, stderr in red. **Edits** show their diff; approvals and errors are never
  folded.
- A plan update is the working area's plan, not a row.

`Ctrl+O` (or `/verbose`) switches to the full view, which shows every call,
its output and the thinking text, and back.

To open just one row, click it: a lookup row opens to its calls and their
output, a `Thought for` line to its text, a `… +N lines` marker to the whole
output. Click it, or any line it opened to, again to close it. From the
keyboard, as in Codex: `F4` puts the focus on the last row on screen
(drawn reversed), `↑`/`↓` move it, `Enter` opens or closes the row, `→`
opens and `←` closes it, and `Esc` or `F4` leaves.

**Focus view** (`/focus`, as Claude Code's) folds further: a finished turn
shows its prompt, one row, `▸ Worked for 2m 5s · 17 tool calls · 2 files +40
−12`, and its answer. The row opens as any other. It's off by default, as
in Claude Code; `/focus` keeps the choice for new sessions, and `[tui]
view_mode` (`default`, `verbose` or `focus`) picks the view a session
starts in.

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

In Auto, Koda asks before a destructive command, a command that asks to
leave the sandbox, and a file outside the project. The menu area shows the
tool name, the action detail, and hotkeys:

```
  Bash  rm -rf dist/
  [y] approve  [n] reject  [f] feedback  [esc] stop
```

For a command leaving the sandbox with a `prefix_rule` (`git commit`, say),
or a web page, `[s] this session` also appears: yes, and don't ask again this
session for commands with that prefix, or for that site. See
[Allow for this session](./approval.md#allow-for-this-session).

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
