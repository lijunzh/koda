# Memory

Memory files persist facts and preferences across sessions.

**Project memory** — `MEMORY.md` in the project root (Koda also reads
`CLAUDE.md` and `AGENTS.md` for compatibility). Injected into every
system prompt for that project.

**Global memory** — `~/.config/koda/memory.md`. Injected into every
system prompt across all projects.

The system prompt carries at most 16,000 characters of memory, global and
project together, because every model call pays for it. Past that it is cut
with a note telling the model to use `MemoryRead` for the rest and to ask you
to trim the file. Keep rarely needed detail in other files.

Koda treats these files as instructions: they override its default guidance,
and what you say in the session overrides them.

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
