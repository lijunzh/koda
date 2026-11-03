# Memory

Memory files persist facts and preferences across sessions.

**Project memory** — `MEMORY.md` in the project root (Koda also reads
`CLAUDE.md` and `AGENTS.md` for compatibility). Sent with every request
for that project.

**Global memory** — `~/.config/koda/memory.md`. Sent with every request
across all projects.

Memory opens the first user message, not the system prompt, as in Claude
Code and Codex. The model is told it is the project's instructions and tips,
ranked above Koda's built-in guidance and below what you ask for. A request
carries at most 16,000 characters of memory, global and
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
/memory                 ← show the paths to both memory files
/memory add <text>      ← append to the project memory file
/memory global <text>   ← append to the global memory file
```

## MemoryWrite tool

The `MemoryWrite` tool lets the model append facts to memory directly
during a conversation:

```text
> Remember that we use tabs not spaces in this project
```

Koda will call `MemoryWrite` automatically when you ask it to remember.

## File precedence

| File | Scope | Priority |
|------|-------|----------|
| `MEMORY.md` | Project root | Highest |
| `CLAUDE.md` | Project root | Used if there is no `MEMORY.md` (compatibility) |
| `AGENTS.md` | Project root | Used if neither is there (compatibility) |
| `~/.config/koda/memory.md` | Global | Base |
