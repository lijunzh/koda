# Sessions and the window

## The top bar

The top bar drags the window; on a Mac the window's own buttons sit
in it. Beside them are three of the app's: hide or show the session list
(Cmd+B), and **Back** and **Forward** (Cmd+[ and Cmd+]), which step
through the sessions you were in, in the order you used them, as a
browser does with pages. Moving to another pane of a split view and
opening the new-session screen count too. The app remembers whether the
list was hidden.

## The session list

The session list, on the left, shows every folder's sessions, the most
recently active first. A small spinner marks a session whose turn is
running, and *needs you* one that waits for an approval or an answer. The
list stays current when the TUI or another window starts, renames or
deletes a session. **Search** narrows it by name. Hover a session for its
**…** menu (or right-click it): open it, open it in split view, rename,
fork, [archive](#archiving-a-session) or delete it.

Renaming, forking and deleting are also in the menu beside the session's
title ([Working in a session](./desktop-conversation.md#the-sessions-title-and-its-menu)).
Delete asks first, and can't be undone; archive can.

## Starting a session

**New session** (Cmd+N) opens an empty session with the prompt box. Under
the box, pick the folder Koda works in (one you've used before, or
**Open folder…**), the model and the mode. The session starts with your
first prompt. A session always works in the folder it started in.

The model menu also lists other providers' models under **More models**, so
a new session can start on any provider you've set up.

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

## With the terminal

The app and the TUI show the same sessions, through the same
[background server](./background-server.md). A session started in the app
is in `koda resume`'s list; one started in the TUI appears in the app's
list as it starts. Both can have one session open at once, and each shows
what happens in the other.
