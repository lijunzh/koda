# Working in a session

## The conversation

The conversation streams in as Koda works. What Koda does between two
messages (reads, searches, commands, edits) folds into one quiet line such
as "Read 3 files, ran 2 commands, edited 1 file"; click it to see each
call and what it printed. Long output folds to its first lines; click to
see the rest. A reply and each code block have a **Copy** button.

There are three views: Normal, Thinking (the model's thoughts too, under "Thought
process") and Verbose (every tool call on its own). Ctrl+O cycles them; they're also under
**Transcript view** in the session's menu.

The model's plan shows as a checklist above the prompt box while a turn
runs; Ctrl+T hides or shows it.

## The prompt box

Enter sends, Shift+Enter adds a line. While a turn
runs, Enter adds your message to it, and Stop (or Esc) ends it. An added
message waits above the box until Koda reads it, at its next step. If the
turn is stopped or fails first, the message comes back to the box, in front
of anything you typed meanwhile.

You can type as soon as a session shows. If you press Enter while it is
still opening, the line above the box says so, and the message is sent once
the session is open. Esc, or changing the text, takes it back. If the session
can't be opened, the text stays in the box and the reason shows.

Under the box are the session's model and mode:

- **The model**, with its effort beside it, opens a menu: the provider's
  models (a number key picks one), **Effort**, and **More models** for the
  other providers you can use. A session keeps the provider it started
  with, so picking another provider's model opens a new session on it, in
  the same folder. Cmd+Shift+I opens the menu.
- **The mode**: Auto makes edits and runs sandboxed commands without asking
  (switching to it asks once to confirm); Plan only reads and plans. See
  [Trust modes](approval.md). Cmd+Shift+M opens the menu.

## Approvals and questions

When Koda needs an approval, asks a question, reaches its step limit or an
MCP server asks for something, a card marked **Needs your input** appears
above the prompt box. A question lists its choices one per row, with
**Other** to type your own; pick one and press **Submit** (Cmd+Enter), or
**Skip**. Answer it there, or in the TUI: whichever answers first wins, and
the other withdraws its card. The app sends a system notification when its
window isn't in front.

## The session's title and its menu

Click the session's title to rename it. The caret beside it opens the
session's menu: **Rename**, **Fork** (a copy to go on in, leaving this one
as it is), **Mark confidential** (a lock beside the title shows it is;
[what that does](./desktop-sessions.md#confidential-sessions)),
**Transcript view**, **Compact**, **Archive** and **Delete** (after asking).

## The side panel and the Activity panel

Two buttons at the top right open what sits beside the conversation. The
**side panel** (Cmd+.) holds panes as tabs; with none open it lists what you
can open: **Changes** (Shift+Cmd+D) and **Background tasks**. The
**Activity panel** is the session's details; a narrow pane leaves it out,
and it steps aside while the side panel is open unless the window is wide:

| Card | What it shows |
|---|---|
| Environment | The folder the session works in, its provider and model |
| Context | How much of the model's context window is used, and **Compact session**, which summarizes the conversation so far to free it |
| Changes | The files the last changes touched; click one to [review it](./desktop-changes.md) |
| Goal | What the session works toward, with an optional token budget |
| Background | Commands running in the background, each with Stop |
| MCP servers | The servers Koda is connected to |
