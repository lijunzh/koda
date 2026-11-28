# Session management

Koda stores every conversation in a local SQLite database, organised by
project root. Each session gets a UUID that you can use to resume it.

```bash
# List and pick a session interactively
/resume

# Resume by ID prefix from the TUI
/resume a3f8bc

# Resume from the command line, as `codex resume` does: pick from the list,
# the most recent with --last, or a full ID or the start of one
koda resume
koda resume --last
koda resume a3f8bc "continue where we left off"

# The same without the TUI, as `codex exec resume`
koda exec resume --last "continue where we left off"
koda exec resume a3f8bc "continue where we left off"

# Fork a session: a copy you continue, the original left as it was, as
# `codex fork` does: pick from the list, --last, or an ID
koda fork
koda fork --last "try it the other way"
koda fork a3f8bc

# The same without the TUI, as `codex exec fork`: by ID; no prompt only forks
koda exec fork a3f8bc "try it the other way"

# The older flag still works: koda -s <ID>, koda exec -s <ID> "..."

# Archive a session: kept, but out of the lists; and bring it back
koda archive a3f8bc
koda unarchive a3f8bc

# Keep this session's text from models on remote providers, and undo that
/confidential
/confidential off

# What models on remote providers are told of it instead: see, accept a
# local model's draft, write your own, remove
/confidential brief
/confidential brief accept
/confidential brief Planning a household budget
/confidential brief clear

# Delete a session permanently
/resume delete a3f8bc
```

**`--last`** is the session with the latest activity in this project, as
Codex sorts by update time; a session where nothing was sent doesn't count.

**Typing in the picker searches**, as in Claude Code's. The ten sessions it
lists are narrowed at once by name, first prompt, id or date. From the
second character Koda also looks through what was said in every session of
the project (your prompts, the replies, and tool calls' names and
arguments), and lists the sessions that mention the text under the others,
each with the words around the first match where the date would be.
Backspace takes a character back; Enter opens the selected session. The
picker searches this project only.

## What a resumed session shows

Its earlier turns, drawn as they were live, as Codex's TUI replays a
resumed thread: your prompts, the replies, thinking and tool calls folded
as they were (`Ctrl+O` unfolds them), a sub-agent's `Done (N tool uses · Ns)`, what
happened to a refused or blocked call, and how long each turn worked. A
`session resumed` line follows them, and a banner when the last turn was
interrupted; type `continue` to pick it up. `koda resume`, `koda -s`,
`/resume` and the sessions picker all show them.

Sessions stored before 0.6.5 are rebuilt from their messages, so they show
less: no reasoning, and no time on a sub-agent's summary.

## Forking a session

`/fork` copies the session you're in and switches to the copy, as Codex's
`/fork` does; `koda fork` does the same from the command line. The copy has
the whole history, its turns drawn as they were, and the same provider,
model and mode (`koda fork --model` names another model, as
`koda resume --model` does). It is named "Fork of" the original's name, and
the original stays as it was, in `/resume`. Over the protocol,
`session/fork` copies a stored session and opens the copy, as Codex's
`thread/fork` does.

## Archiving a session

`koda archive <SESSION>` puts a session away without deleting it, as
`codex archive` does. Nothing stored changes, and `koda unarchive <SESSION>`
brings it back.

- **What archiving hides:** the session leaves `/resume`, `koda resume`'s
  list and `--last`, and the start of its ID no longer finds it. Resuming it
  by its full ID is refused, with the `koda unarchive` command to run first.
- **A session that is open is closed first.** A turn running in it stops and
  is saved as interrupted, and a question or approval it was waiting on is
  withdrawn. A window that had it open says it was archived; after
  `koda unarchive`, the next prompt there goes on with it.
- **Sub-agents' sessions go with it,** and come back with it. A fork is a
  conversation of its own and stays.
- **Forking still works:** `koda fork <full ID>` copies an archived session
  into a new one, which isn't archived.
- **`SESSION`** is the session's ID or its first characters. A full ID names
  any session; a shorter one is looked for among the current directory's
  sessions (`koda -C <DIR> archive …` for another's), the ones that aren't
  archived for `koda archive` and the archived ones for `koda unarchive`.
  Sessions aren't found by name.

Both commands use the [background server](./background-server.md), starting
it if needed, so a session open in a window is closed there; `--no-daemon`
runs them in their own process instead. The TUI has no command for this yet.
Over the protocol they are `session/archive` and `session/unarchive`, Codex's
`thread/archive` and `thread/unarchive` ([Archiving](./serve.md#archiving-a-session)).

## Confidential sessions

Koda's model can look through your other sessions
([`SearchSessions` and `ReadSession`](./tools.md#other-sessions)), so what
you said to a model on your own machine could otherwise be read by a session
running on a remote provider and sent there. Mark a session **confidential**
to stop that: `/confidential` in the TUI, or **Mark confidential** in the
desktop app's session menus. `/confidential off` and **Unmark confidential**
take the mark off. Only you set it; Koda never guesses that a session is
sensitive.

- **What the mark does:** the session's text is given only to a
  [local model](#what-counts-as-a-local-model). From a session on any
  other, `SearchSessions` never returns it and `ReadSession` returns no
  text of it. That model is told only that confidential sessions exist: for
  the folder it works in, up to five lines of the form
  `Confidential session 1a2b3c4d, last active 2026-11-20`, then how many
  more there are, and that they can be read from a session on a local model
  or after you unmark them. The notice is the same whatever was searched
  for, and says nothing taken from a session: no title, no text, no folder.
  A line ends with the session's [brief](#the-brief) only once you have
  accepted one. No argument of either tool changes this.
- **A session that reads one becomes confidential.** When a session on a
  local model is given a confidential session's text, by a search result
  or a read, Koda marks that session too, before the text is handed over:
  from then on its own transcript holds the text. The tool's result says
  so, and so does the TUI. You can unmark it like any other.
- **Opening it on another provider is refused.** A confidential session
  resumes and forks on a local model, or on the provider and endpoint it
  already has, which has seen its text already. `koda fork` with another
  remote `--provider`, or `koda resume` or `koda fork` with another
  `--base-url` that isn't local, is refused, and says to unmark the session
  first.
- **Its sub-agents run where it may.** In a confidential session a
  sub-agent starts only on a local model or on the session's own provider
  and endpoint. One [configured](./agents.md) for another is refused when
  the model asks for it, and the model is told why, so it can pick another
  or do the task itself. The mark is read each time, so this holds from the
  moment a session becomes confidential.
- **Its model saves no notes.** `MemoryWrite` refuses in a confidential
  session, a sub-agent's too: Koda's notes are read by every session of the
  project, on any provider. Your own `/memory add` saves as before.
- **Forks are marked with it.** Marking a session marks the sessions
  already forked from it, and theirs, since each holds a copy of the
  conversation; `/confidential` says how many, and each gets its lock. A
  fork made afterwards is confidential from the start, and so are the
  sessions of the sub-agents it runs. Unmarking is one session at a time:
  a fork stays confidential until you unmark it.
- **Your own view doesn't change.** `/resume`, its search, the desktop
  app's list and Search show a confidential session like any other, with a
  lock on its row. The mark is about what a model is given.

### The brief

An id and a date tell a model on a remote provider nothing about a
confidential session. A **brief** is one line you let it be told in place
of the session's text: what kind of work the session is, with no details,
such as "Planning a household budget". With a brief, the session's line in
`SearchSessions`'s notice and in `ReadSession`'s answer reads

```text
Confidential session 1a2b3c4d, last active 2026-11-20: Planning a household budget
```

and without one it ends at the date.

- **A local model drafts it, never a remote one.** Koda asks a
  [local model](#what-counts-as-a-local-model) for one line of at most
  twelve words that names no person, company, project or file and holds no
  number, path, identifier or quotation. It is given what you and the
  assistant said in the session, your messages first, up to about 8,000
  characters, and no tool call or output.
- **No model is shown a draft.** The draft is stored apart from the brief
  and waits for you. The TUI says when one arrives for the session you are
  in: `/confidential brief` shows it, `/confidential brief accept` makes it
  the brief, `/confidential brief <text>` sets your own words and
  `/confidential brief clear` removes the brief. In the desktop app the
  draft waits under the session's title with **Accept**, **Edit** and
  **Discard**, and **Edit brief…** is in the title's menu.
- **A draft with a detail in it is thrown away,** not shown to you: one
  that holds a path, a file's or a host's name, a URL, an email address,
  or four digits in a row. Koda tells this from the line's shape, so it
  can't know a name from a word: read a draft before you accept it. A
  brief you type yourself is yours, and is checked only for being one line
  of 100 characters at most.
- **Koda loads no model for it.** A draft is asked only of a model that
  has just answered a turn:
  - **When you mark a session** that is idle on a local model, and that
    model has answered a turn since the session was opened, it is asked at
    once.
  - **After a turn ends on a local model** and its session is idle, that
    model is asked for up to two of the sessions still waiting for a
    draft, in any folder, the most recently active first (not after a
    `koda exec` run, which is over with its turn). A confidential
    session on a remote provider, or one marked while no local model was
    in use, gets its draft this way, the next time you use a local model.

  Until then the session's line is its id and its date.
- **It keeps out of a turn's way.** One draft is asked for at a time. None
  is asked while a turn runs at that model's endpoint in the same Koda,
  and a turn that starts there stops the request, which is made again
  after a later turn. A request gets 30 seconds. A failed one is made
  again after later turns, three times in all, and a draft thrown away
  once more, until Koda (or the background server) restarts.
- **Discarding is your answer.** After you discard a draft or clear a
  brief, Koda drafts no other for that session until it restarts, or you
  unmark the session and mark it again.
- **It goes with the mark.** Unmarking a session removes its brief and its
  draft. A fork starts with neither and gets a draft of its own.

A brief is told to remote providers: that is what it is for. Don't put in
it what you marked the session to keep.

### What counts as a local model

A model is local when both hold, read from what the session was started
with, with no lookup of names:

- **Its provider is a local kind:** LM Studio, Ollama, vLLM or llama.cpp.
  A provider Koda doesn't know counts as remote.
- **Its endpoint is on your own machine or network.** The host in its URL
  (`http` or `https`) is one of:

  | Host | Examples |
  |---|---|
  | This machine | `localhost`, `127.0.0.1`, `[::1]` |
  | A private address | `10.x.x.x`, `172.16.x.x` to `172.31.x.x`, `192.168.x.x` |
  | A link-local or unique-local address | `169.254.x.x`, `fe80::…`, `fd…` |
  | A carrier-grade-NAT address, which Tailscale uses | `100.64.0.0` to `100.127.255.255` |
  | A name of one label | `studio` |
  | A name ending in `.local`, `.localhost`, `.lan`, `.home.arpa` or `.ts.net` | `studio.local`, `studio.tailnet.ts.net` |

  Anything else is remote: a public address or name, a URL Koda can't
  read, and an LM Studio server reached through a public host name. A
  session with no endpoint recorded uses its provider's own, which for
  these four is this machine.

### What the mark doesn't do

- **It isn't encryption.** The session is stored as before, in plain text
  in `~/.config/koda/db/koda.db`, readable by you and by anything running
  as you. The sandbox keeps the model's commands out of that folder.
- **It isn't retroactive.** A session on a remote provider that read this
  one before you marked it keeps what it read.
- **A feedback report holds the session.** `/feedback` with history saves
  the session's stored rows, what it read from a confidential session
  included, in a ZIP you choose whether to share.
- **It doesn't stop the session itself.** A session marked while it runs
  on a remote provider goes on sending its own conversation there, as it
  always has. The mark covers what other models can read.

Over the protocol it is `session/confidential/set`
([Marking a session confidential](./serve.md#marking-a-session-confidential)),
and the brief is `session/confidential/brief/set`.

## Session title

In the TUI, the first 50 characters of your first message become the session's
title, shown in `/resume`. `/rename <name>` names it instead, as Codex's
`/rename` does; a name you gave stays.
Over the protocol, `session/name/set` does the same and
`session/name/updated` follows.

## A session in its own worktree

`koda --worktree` (or `koda exec --worktree "…"`) starts the session in a new
Git worktree, as Codex's `--worktree` does, so its edits stay apart from your
checkout and from other sessions':

- **Where:** `~/.config/koda/worktrees/<id>/<project>`, outside the
  repository. Koda prints the path when it starts.
- **What:** your project's `HEAD`, detached: Koda makes no branch. Uncommitted
  changes and untracked files stay in your checkout. Commit or branch in the
  worktree as you would anywhere: `git switch -c my-change`.
- **Whose:** the session that ran there is recorded in the worktree's git
  metadata.
- **Afterwards:** Koda keeps the worktree and prints how to remove it once
  you're done: `git worktree remove <path>`, run from your repository. Git
  refuses while the worktree holds changes; don't add `--force` unless you
  mean to lose them.

To carry a conversation into a worktree, fork it there, as
`codex fork <id> --worktree` does:

```sh
koda fork a3f8bc --worktree "now build it"
koda exec fork a3f8bc --worktree "now build it"   # without the TUI
```

The copy opens in a new worktree of the repository the session worked in,
wherever you run the command (`--project-root` names another), with the whole
history; it belongs to the worktree, so `koda resume` there lists it, and the
original stays in your checkout. The worktree has your project's `HEAD`, so
uncommitted edits the earlier turns made aren't in it, though the history
mentions them: commit first to bring them along. The session needs naming:
`--last` and the list don't go with `--worktree`. `koda resume` refuses
`--worktree`; fork the session instead.

