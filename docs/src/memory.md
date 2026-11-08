# Memory

Koda sends two kinds of memory with every request for a project: the
instruction files you write, and the notes Koda keeps for itself.

**Instruction files.** `AGENTS.md`, the file Codex, Qwen Code and most
other agents read. Koda reads one per folder, from the git root down to the
project, root first:

- `AGENTS.override.md`, if you want a folder's file replaced without editing it;
- else `AGENTS.md`;
- else `CLAUDE.md`, so a project set up for Claude Code still works.

A folder with both `AGENTS.md` and `CLAUDE.md` gives Koda only `AGENTS.md`.
Your own instructions for every project go in `~/.config/koda/AGENTS.md`
(`$XDG_CONFIG_HOME/koda/AGENTS.md` when that's set), sent first.

**Koda's notes.** What Koda saves with `MemoryWrite` or `/memory add` goes to
`~/.config/koda/memories/<project>/MEMORY.md`, never into the project, where
it would land in a file your team reads. `/memory global` and
`MemoryWrite` with `scope: "global"` go to `~/.config/koda/memories/MEMORY.md`.
`<project>` is the project's path with each `/` turned into `-`. The notes
are sent after the instruction files.

**Older names.** A project `MEMORY.md` and a global `~/.config/koda/memory.md`
are still read where nothing newer exists, with a warning to rename them to
`AGENTS.md`. Notes Koda wrote into a project file before this stay there as
part of that file; move them if you'd rather they were private. The
instruction files, then the notes, share the 32,768-character cap below, so
when the files are long the notes are what gets cut; `MemoryRead` still
returns everything.

Memory opens the first user message, not the system prompt, as in Claude
Code and Codex. The model is told it is the project's instructions and tips,
ranked above Koda's built-in guidance and below what you ask for. A request
carries at most 32,768 characters of memory, global and
project together, because every model call pays for it. Past that it is cut
with a note telling the model to use `MemoryRead` for the rest and to ask you
to trim the file. Keep rarely needed detail in other files.

Koda treats these files as instructions: they override its default guidance,
and what you say in the session overrides them.

## Keep it short, especially for small models

The model reads the whole file on every call, and a small model pays for
noise in it. In Koda's evals, one task's checkout came with a 24,000-character
`CLAUDE.md` holding a file tree, CI details and notes on the maintainer's
workflow. With that file loaded, Haiku passed about half its runs, against
about 9 in 10 without it. That was true under both Koda and Claude Code.
Sonnet passed every run with the file loaded.

So put in what the model can't easily find for itself and needs on most
turns:

- How to build, test and lint, with the exact commands.
- Conventions the code doesn't show, and rules that must not be broken.
- Where to look for the rest: a design doc, a contributing guide.

Leave out what it can read from the project when it needs it, such as a file
tree, a list of tests or CI internals. Leave out history and anything that
matters only now and then. If you work with a small or local model, keep it
to a page.

```text
/memory                 ← show the instruction file and where the notes are
/memory add <text>      ← add to Koda's notes for this project
/memory global <text>   ← add to Koda's global notes
```

## MemoryWrite tool

The `MemoryWrite` tool lets the model add to its notes during a
conversation:

```text
> Remember that we use tabs not spaces in this project
```

Koda will call `MemoryWrite` automatically when you ask it to remember.

## Order

| File | Where | Sent |
|------|-------|------|
| `AGENTS.override.md`, else `AGENTS.md` | Koda's config directory | First |
| `AGENTS.override.md`, else `AGENTS.md`, else `CLAUDE.md` | Each folder from the git root down | Next, root first |
| `memories/MEMORY.md` | Koda's config directory | Then Koda's global notes |
| `memories/<project>/MEMORY.md` | Koda's config directory | Last, Koda's notes for this project |
