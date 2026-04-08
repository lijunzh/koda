---
name: remember
description: Review auto-memory entries and propose promotions, cleanups, and deduplication across all memory layers.
tags: [memory, review, cleanup, organisation]
when_to_use: Use when you want to review, organise, or promote auto-memory entries. Also useful for cleaning up outdated or conflicting entries across MEMORY.md, global memory, and auto-memory.
allowed_tools: [Read, Write, Edit, Bash, MemoryRead, MemoryWrite]
user_invocable: true
---

# Memory Review

## Goal

Review koda's memory landscape and produce a clear report of proposed changes, grouped by action type. Do **NOT** apply changes — present proposals for user approval first.

## Step 1: Gather All Memory Layers

Read every active memory layer:

1. **Auto-memory** — already in your system prompt context; review it there
2. **Project memory** — read whichever file exists at the project root (first match wins):
   - `MEMORY.md` — koda native
   - `CLAUDE.md` — Claude Code compat
   - `AGENTS.md` — Codex compat
3. **Global memory** — read `~/.config/koda/memory.md`

```bash
# Check which project memory file is active
ls MEMORY.md CLAUDE.md AGENTS.md 2>/dev/null | head -1
```

**Success criteria**: you have the contents of all three layers and can compare them.

## Step 2: Classify Each Auto-Memory Entry

For each substantive entry in auto-memory, determine the best destination:

| Destination | What belongs there | Examples |
|---|---|---|
| **MEMORY.md** (project) | Project conventions and instructions that all contributors working with koda in this repo should follow | "use `cargo nextest` not `cargo test`", "API routes use kebab-case", "always run `cargo clippy` before committing" |
| **Global memory** (`~/.config/koda/memory.md`) | Personal preferences specific to this user, not tied to any one project | "I prefer concise responses", "always explain trade-offs", "don't auto-commit", "run tests before committing" |
| **Stay in auto-memory** | Working notes, temporary context, or entries that don't clearly fit elsewhere | Session-specific observations, uncertain patterns, one-off notes |

**Important distinctions:**
- `MEMORY.md` and global memory contain instructions for koda, not preferences for external tools (editor theme, IDE keybindings, shell aliases don't belong in either)
- Workflow practices (PR conventions, merge strategy, branch naming) are ambiguous — ask the user whether they're personal or project-wide
- When unsure, ask rather than guess

**Success criteria**: every entry has a proposed destination, or is flagged as ambiguous.

## Step 3: Identify Cleanup Opportunities

Scan across all layers for:

- **Duplicates**: auto-memory entries already captured in `MEMORY.md` or global memory → propose removing from auto-memory
- **Outdated**: entries in `MEMORY.md` or global memory contradicted by newer auto-memory entries → propose updating the older layer
- **Conflicts**: contradictions between any two layers → propose resolution, noting which is more recent
- **Bloat**: very long auto-memory entries that could be summarised without losing meaning

**Success criteria**: all cross-layer issues identified.

## Step 4: Present the Report

Output a structured report grouped by action type:

1. **Promotions** — entries to move, with destination and rationale
2. **Cleanup** — duplicates, outdated entries, conflicts to resolve
3. **Ambiguous** — entries where you need the user's input before acting
4. **No action needed** — brief note on entries that should stay put

If auto-memory is empty, say so and offer to review `MEMORY.md` and global memory for stale content.

## Rules

- Present **ALL** proposals before making any changes
- Do **NOT** modify files without explicit user approval for each change
- Do **NOT** create new memory files unless the user confirms they want one
- Ask about ambiguous entries — do not guess
