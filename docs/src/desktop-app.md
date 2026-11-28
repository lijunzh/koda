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

- **The top bar** drags the window; on a Mac the window's own buttons sit
  in it. Beside them are three of the app's: hide or show the session list
  (Cmd+B), and **Back** and **Forward** (Cmd+[ and Cmd+]), which step
  through the sessions you were in, in the order you used them, as a
  browser does with pages. Moving to another pane of a split view and
  opening the new-session screen count too. The app remembers whether the
  list was hidden.
- **The session list**, on the left, shows every folder's sessions, the most
  recently active first. A small spinner marks a session whose turn is
  running, and *needs you* one that waits for an approval or an answer. The
  list stays current when the TUI or another window starts, renames or
  deletes a session. **Search** narrows it by name. Hover a session for its
  **…** menu (or right-click it): open it, open it in split view, rename,
  fork, [archive](#archiving-a-session) or delete it.
- **New session** (Cmd+N) opens an empty session with the prompt box. Under
  the box, pick the folder Koda works in (one you've used before, or
  **Open folder…**), the model and the mode. The session starts with your
  first prompt. A session always works in the folder it started in.
- **The conversation** streams in as Koda works. What Koda does between two
  messages (reads, searches, commands, edits) folds into one quiet line such
  as "Read 3 files, ran 2 commands, edited 1 file"; click it to see each
  call and what it printed. Long output folds to its first lines; click to
  see the rest. A reply and each code block have a **Copy** button.
- **Views**: Normal, Thinking (the model's thoughts too, under "Thought
  process") and Verbose (every tool call on its own). Ctrl+O cycles them; they're also under
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

Two buttons at the top right open what sits beside the conversation. The
**side panel** (Cmd+.) holds panes as tabs; with none open it lists what you
can open: **Changes** (Shift+Cmd+D) and **Background tasks**. The
**Activity panel** is the session's details; a narrow pane leaves it out,
and it steps aside while the side panel is open unless the window is wide:

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
MCP server asks for something, a card marked **Needs your input** appears
above the prompt box. A question lists its choices one per row, with
**Other** to type your own; pick one and press **Submit** (Cmd+Enter), or
**Skip**. Answer it there, or in the TUI: whichever answers first wins, and
the other withdraws its card. The app sends a system notification when its
window isn't in front.

## Reviewing changes

**Changes** opens as a pane in the side panel (Shift+Cmd+D, the side
panel's list, or a file in the Activity panel's Changes card) with what a
turn changed. Each file is a row with
its added and removed counts; click the row to fold it. Unchanged lines
between the changes are folded to a count, and a bar at the left marks each
added or removed line. While a turn runs the pane updates as Koda edits;
for an earlier turn, pick it from the **Turn** menu at the top.

To comment, hover a line and click the **+** beside its number. Enter adds
the comment. Cmd+Enter, or **Send**, sends all your comments to Koda as one
message, each with its file, line number and the line itself. If a turn is
running, they're added to it; otherwise they start the next turn.

The pane shows the edits Koda's Edit and Write tools made. Files a command
changed (`sed`, a formatter, `git checkout`) and a sub-agent's edits aren't
in it, as in Codex's turn diff.

## Side by side

To work in two sessions at once, Option-click a session in the list, choose
**Open in split view** from its menu, or drag it onto the window. Each pane
is a full session: its own conversation, prompt box and Changes pane. The
pane you're not working in steps back a little. Clicking a session in the
list opens it in the pane you last used; a session that's already open goes
to its pane. ✕, or Cmd+Ctrl+W, takes a session out of split view. Up to four
sessions fit side by side.

## Archiving a session

**Archive**, in a session's **…** menu or the menu beside its title, puts a
session away without deleting it. It leaves the session list, here and in
the TUI, and its pane closes. Archiving doesn't wait: a turn running in the
session stops, saved as interrupted. The sessions of its sub-agents go with
it.

**Archived**, under Search, shows the sessions put away, the most recent
hundred. Click one to read it: its conversation and changes show, with
**Unarchive** where the prompt box would be. **Unarchive** (there, in the
session's menu, or in its **…** menu in the Archived list) brings it back
to the session list, ready to go on. Click **Archived** again to return to
the session list.

`koda archive` and `koda unarchive` do the same from a terminal
([Sessions](./sessions.md#archiving-a-session)).

## Settings

The gear at the bottom of the session list, or Cmd+,, opens Settings:

| Section | Setting |
|---|---|
| Appearance | **Theme**: follow the system (the default), or always light or dark. **Transcript width**: narrow (the default) or wide. Motion follows the system's Reduce motion setting |
| Conversation | **Default transcript view**: Normal, Thinking or Verbose when the app opens. Ctrl+O still changes it at any time |
| Desktop app | **Notifications**: say when Koda needs you or has finished while the app isn't in front. **Keep computer awake**: keep the Mac from idle-sleeping while the app is open, so a long turn can finish (the display can still turn off). The app's and the server's versions |
| Keyboard shortcuts | The app's shortcuts |

They're remembered between launches. Keys, providers, the default model and
everything else about Koda itself are still set with `koda` (`/key`,
`/provider`, `config.toml`).

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
