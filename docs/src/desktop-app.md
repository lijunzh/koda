# The desktop app

Koda's desktop app is a window onto the same [background server](background-server.md)
as the TUI. A session is in both at once: start it in the app, go on in the
terminal with `koda resume`, or the other way round. The app follows Claude
Code's desktop app in what you see and do, and Codex in how it talks to the
server.

It runs on Apple Silicon Macs.

## Install and open it

```bash
brew install koda
koda app
```

`koda app` puts the app in `~/Applications/Koda.app` the first time, and
again after each `brew upgrade koda`, so the app and `koda` are always one
version. After that you can open it from Launchpad or Spotlight like any app.

The app is signed ad hoc, not with an Apple Developer ID. macOS opens it
because `koda app` installs it; a copy downloaded with a browser would be
quarantined and refused.

The app carries its own `koda`, and uses it to start the background server
when none is running, just as `koda` does. It reads your login shell's `PATH`,
so the commands Koda runs find the same tools as in your terminal. It never
reads your API keys or config: set those with `koda` (`/key`, `/provider`).

## The window

- **The session list**, on the left, shows every folder's sessions, the most
  recently active first. Next to a session, *working* means a turn is
  running, *needs you* means it waits for an approval or an answer, and
  *open* means it's loaded on the server, ready. The list stays current when the TUI or
  another window starts, renames or deletes a session.
- **New session** (Cmd+N) opens an empty session with the prompt box. Under
  the box, pick the folder Koda works in (one you've used before, or
  **Open folder…**), the model and the mode. The session starts with your
  first prompt. A session always works in the folder it started in.
- **The conversation** streams in as Koda works. Lookups (reads, searches,
  listings) fold into one row such as "Read 3 files, searched 2 patterns";
  click it to see each call and what it printed. Long output folds to its
  first lines; click to see the rest.
- **Views**: Normal, Thinking (the model's thoughts too) and Verbose (every
  tool call on its own). Ctrl+O cycles them; they're also under
  **Transcript view** in the session's menu.
- **The prompt box**: Enter sends, Shift+Enter adds a line. While a turn
  runs, Enter adds your message to it, and Stop (or Esc) ends it.

## Under the prompt box

- **The model**, with its effort beside it, opens a menu: the provider's
  models (a number key picks one), **Effort**, and **More models** for the
  other providers you can use. A session keeps the provider it started
  with, so picking another provider's model opens a new session on it, in
  the same folder. Cmd+Shift+I opens the menu.
- **The mode**: Auto makes edits and runs sandboxed commands without asking
  (switching to it asks once to confirm); Plan only reads and plans. See
  [Trust modes](approval.md). Cmd+Shift+M opens the menu.

## The session's title and its panel

Click the session's title to rename it. The caret beside it opens the
session's menu: **Rename**, **Fork** (a copy to go on in, leaving this one
as it is), **Transcript view**, **Compact** and **Delete** (after asking).

At the right of the conversation is the session's details panel; the button
at the top right hides or shows it, and a narrow pane leaves it out:

| Card | What it shows |
|---|---|
| Environment | The folder the session works in, its provider and model |
| Context | How much of the model's context window is used, and **Compact session**, which summarizes the conversation so far to free it |
| Changes | The files the last changes touched; click one to review it (below) |
| Goal | What the session works toward, with an optional token budget |
| Background | Commands running in the background, each with Stop |
| MCP servers | The servers Koda is connected to |

The model's plan shows as a checklist above the prompt box while a turn
runs; Ctrl+T hides or shows it.

## Approvals and questions

When Koda needs an approval, asks a question, reaches its step limit or an
MCP server asks for something, a card appears above the prompt box. Answer
it there, or in the TUI: whichever answers first wins, and the other
withdraws its card. The app sends a system notification when its window
isn't in front.

## Reviewing changes

**Changes** opens a panel beside the conversation with what a turn changed,
file by file, with line numbers. While a turn runs it updates as Koda edits;
for an earlier turn, pick it from the list at the top.

Click a line to comment on it, and press Enter to add the comment. **Send**
sends all your comments to Koda as one message, each with its file, line
number and the line itself. If a turn is running, they're added to it;
otherwise they start the next turn.

The panel shows the edits Koda's Edit and Write tools made. Files a command
changed (`sed`, a formatter, `git checkout`) and a sub-agent's edits aren't
in it, as in Codex's turn diff.

## Side by side

To work in two sessions at once, right-click a session in the list and
choose **Open in split view**, or drag it onto the window. Each pane is a
full session: its own conversation, controls, prompt box and Changes panel.
Clicking a session in the list opens it in the pane you last used; a session
that's already open goes to its pane. ✕ closes a pane. Up to four sessions
fit side by side.

## Updates and the server

The app and the server must be one version. After `brew upgrade koda`:

- Run `koda app` again. It installs the new app and opens it.
- The old app, still open, says Koda was updated and offers to restart.
- If turns are running in the old server, the new app asks whether to wait
  for them or stop them before it replaces the server.

If the server goes away, the app reconnects on its own and starts a new
server if none answers. Closing the app stops nothing: a running turn goes
on in the server, and you can open it again in the app or the TUI. See
[The background server](background-server.md) for the whole picture.

## What it doesn't do yet

- Attach files or images to a prompt.
- Show what a command or a sub-agent changed in Changes.
- Run on Intel Macs, Linux or Windows.
