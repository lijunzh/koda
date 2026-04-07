# Memory

Memory files persist facts and preferences across sessions.

**Project memory** — `MEMORY.md` in the project root (Koda also reads
`CLAUDE.md` and `AGENTS.md` for compatibility). Injected into every
system prompt for that project.

**Global memory** — `~/.config/koda/memory.md`. Injected into every
system prompt across all projects.

```text
/memory        ← show the paths to both memory files
/memory save   ← ask the model to summarise and append to MEMORY.md
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
| `.koda/MEMORY.md` | Project | Highest |
| `MEMORY.md` | Project root | High |
| `CLAUDE.md` | Project root | High (compatibility) |
| `AGENTS.md` | Project root | High (compatibility) |
| `~/.config/koda/memory.md` | Global | Base |
