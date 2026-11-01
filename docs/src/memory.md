# Memory

Memory files persist facts and preferences across sessions.

**Project memory** — `MEMORY.md` in the project root (Koda also reads
`CLAUDE.md` and `AGENTS.md` for compatibility). Injected into every
system prompt for that project.

**Global memory** — `~/.config/koda/memory.md`. Injected into every
system prompt across all projects.

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
