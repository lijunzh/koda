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
deletes a session. Hover a session for its **…** menu (or right-click it):
open it, open it in split view, rename, fork,
[mark it confidential](#confidential-sessions),
[archive](#archiving-a-session) or delete it.

**Search** narrows the list by name as you type. From the second
character it also looks through what was said: under the names that match,
**In conversations** lists the sessions in which you, Koda or a tool call
used the text, each with the words around the first place it occurs. The
text is taken as written, letters in either case; what tools returned and
Koda's thinking aren't searched. Click a result to open the session at
that place. While [**Archived**](#archiving-a-session) is showing, Search
looks through the archived sessions instead.

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

## Confidential sessions

**Mark confidential**, in a session's **…** menu, the Archived list's and
the menu beside its title, keeps the session's text from models on remote
providers: when another session's model searches or reads your sessions,
only one on a local model (LM Studio, Ollama, vLLM or llama.cpp, on your
own machine or network) gets this one. A lock shows on the session's row
and beside its title; hover it for what it means. Sessions forked from it
get the lock too. **Unmark confidential** takes it off that session
alone. A session on a local
model that reads a confidential one gets the lock too, as it happens.
Your own list and Search show the session as before, and it is still
stored in plain text on this machine
([Sessions](./sessions.md#confidential-sessions) has the whole rule).

A model on a remote provider is told only that a confidential session
exists, unless you give the session a **brief**: one line that says what
kind of work it is, such as "Planning a household budget". A local model
drafts one after a turn, never a remote one, and the draft waits in a quiet
line under the session's title: *Brief for sessions on remote models:
"…"*, with **Accept**, **Edit** and **Discard**. No model is told it until
you accept it. **Edit** turns the line into a box: Enter saves what you
typed as the brief, Esc leaves the draft waiting. **Edit brief…**, in the
menu beside the title of a confidential session, opens the same box on the
brief it has; saved empty, the brief is removed. Hover the lock for the
brief in use. A draft you discard doesn't come back until the background
server restarts or you unmark the session and mark it again
([The brief](./sessions.md#the-brief)).

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
