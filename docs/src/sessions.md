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

# Delete a session permanently
/resume delete a3f8bc
```

**`--last`** is the session with the latest activity in this project, as
Codex sorts by update time; a session where nothing was sent doesn't count.

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

