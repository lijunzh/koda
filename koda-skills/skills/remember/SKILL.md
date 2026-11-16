---
name: remember
description: Review the notes Koda saved and propose promotions, cleanups, and deduplication across all memory layers.
tags: [memory, review, cleanup, organisation]
when_to_use: Use when you want to review, organise, or promote Koda's saved notes. Also useful for cleaning up outdated or conflicting entries across the project's AGENTS.md, your global AGENTS.md, and Koda's notes.
allowed_tools: [Read, Write, Edit, Bash, MemoryRead, MemoryWrite]
user_invocable: true
---

# Memory Review

## Goal

Review koda's memory landscape and produce a clear report of proposed changes, grouped by action type. Do **NOT** apply changes — present proposals for user approval first.

## Step 1: Gather All Memory Layers

Read every active memory layer:

1. **Koda's notes** — what `MemoryWrite` saved, kept outside the project. `MemoryRead` shows them and names the file (`~/.config/koda/memories/<project>/MEMORY.md`, global ones in `~/.config/koda/memories/MEMORY.md`)
2. **Project instructions** — the first of `AGENTS.override.md`, `AGENTS.md` and `CLAUDE.md` at the project root (and in folders between the git root and here)
3. **Global instructions** — `~/.config/koda/AGENTS.md`

```bash
# Which project instruction file is active
ls AGENTS.override.md AGENTS.md CLAUDE.md 2>/dev/null | head -1
```

**Success criteria**: you have the contents of all three layers and can compare them.

## Step 2: Classify Each Auto-Memory Entry

For each substantive entry in Koda's notes, determine the best destination:

| Destination | What belongs there | Examples |
|---|---|---|
| **AGENTS.md** (project) | Project conventions and instructions that all contributors working with koda in this repo should follow | "use `cargo nextest` not `cargo test`", "API routes use kebab-case", "always run `cargo clippy` before committing" |
| **Global AGENTS.md** (`~/.config/koda/AGENTS.md`) | Personal preferences specific to this user, not tied to any one project | "I prefer concise responses", "always explain trade-offs", "don't auto-commit", "run tests before committing" |
| **Stay in Koda's notes** | Working notes, temporary context, or entries that don't clearly fit elsewhere | Session-specific observations, uncertain patterns, one-off notes |

**Important distinctions:**
- `AGENTS.md` and the global `AGENTS.md` contain instructions for agents, not preferences for external tools (editor theme, IDE keybindings, shell aliases don't belong in either)
- Workflow practices (PR conventions, merge strategy, branch naming) are ambiguous — ask the user whether they're personal or project-wide
- When unsure, ask rather than guess

**Success criteria**: every entry has a proposed destination, or is flagged as ambiguous.

## Step 3: Identify Cleanup Opportunities

Scan across all layers for:

- **Duplicates**: notes already captured in `AGENTS.md` or the global `AGENTS.md` → propose removing them from Koda's notes
- **Outdated**: entries in `AGENTS.md` or the global `AGENTS.md` contradicted by newer notes → propose updating the older layer
- **Conflicts**: contradictions between any two layers → propose resolution, noting which is more recent
- **Bloat**: very long notes that could be summarised without losing meaning

**Success criteria**: all cross-layer issues identified.

## Step 4: Present the Report

Output a structured report grouped by action type:

1. **Promotions** — entries to move, with destination and rationale
2. **Cleanup** — duplicates, outdated entries, conflicts to resolve
3. **Ambiguous** — entries where you need the user's input before acting
4. **No action needed** — brief note on entries that should stay put

If Koda's notes are empty, say so and offer to review `AGENTS.md` and the global `AGENTS.md` for stale content.

## Rules

- Present **ALL** proposals before making any changes
- Do **NOT** modify files without explicit user approval for each change
- Do **NOT** create new memory files unless the user confirms they want one
- Ask about ambiguous entries — do not guess
