---
name: create-skill
description: Scaffold a new bundled skill (.koda/skills/<name>/SKILL.md) with correct frontmatter, tool scoping, and a clear when_to_use trigger.
tags: [skill, scaffold, create, meta]
when_to_use: Use when the user asks to create a new skill, capture a workflow as a skill, or extend koda with reusable expert instructions.
allowed_tools: [Read, Write, Glob, List, AskUser]
---

# Create Skill

You are scaffolding a new koda skill. The goal: produce a working
`.koda/skills/<name>/SKILL.md` (or `~/.config/koda/skills/<name>/SKILL.md`)
that the user can immediately invoke via `ActivateSkill` or `/<name>`.

## Principles

- **Read before writing.** Open `koda-core/skills/code-review/SKILL.md`
  or `koda-core/skills/debug/SKILL.md` first to ground yourself in the
  canonical structure before generating the file.
- **Concise is key.** The skill body is loaded into context every time
  the skill is activated. Don't pad. Skip explanations koda already knows
  (it's a code agent, you don't need to explain what `Grep` is).
- **`when_to_use` is the most important field.** It's what the model uses
  to decide whether to auto-activate the skill. Include trigger phrases.
- **Scope tools.** A code-review skill should not have `Write` access. A
  scaffolding skill should not have `Delete`. Use `allowed_tools` to
  enforce least privilege.

## Process

### 1. Clarify intent (if ambiguous)

Use `AskUser` only when genuinely ambiguous. Skip it for obvious requests
like "create a skill that runs the tests." Things worth asking when unclear:

- **Project or personal scope?** Project = `.koda/skills/<name>/`,
  personal/cross-project = `~/.config/koda/skills/<name>/`
- **What triggers it?** Slash command (`/<name>`)? Auto-activated when
  the model sees a matching task? Both?
- **What tools does it need?** Read-only analysis (`Read`, `Grep`,
  `Glob`), or also write (`Write`, `Edit`)?

### 2. Choose the file path

| Scope    | Path                                                    | When                                           |
| -------- | ------------------------------------------------------- | ---------------------------------------------- |
| Project  | `.koda/skills/<name>/SKILL.md`                          | Workflow specific to this repo.                |
| Personal | `~/.config/koda/skills/<name>/SKILL.md`                 | Cross-project workflow that follows the user.  |

**Note:** Personal path is `~/.config/koda/`, **not** `~/.koda/`.

The `<name>` is both the directory name and the slash-command name (e.g.
`code-review` becomes `/code-review`). Use kebab-case.

### 3. Write the SKILL.md

```markdown
---
name: my-skill
description: One-line purpose (shown in skill listing — keep under 200 chars)
when_to_use: Use when the user asks to <trigger>. Example phrases: "<phrase>", "<phrase>".
tags: [topic-a, topic-b]
allowed_tools: [Read, Grep, Glob, List, Bash]
---

# My Skill

You are <role>. <One-sentence statement of the goal>.

## Principles
- <key behavioral constraint #1>
- <key behavioral constraint #2>

## Process
1. <First concrete step>
2. <Second concrete step>
...

## Anti-patterns
- <thing not to do, with reason>
```

### 4. Field reference

**Required frontmatter:**

- `name` — skill identifier; matches the directory name and the
  slash-command name (`/<name>`)
- `description` — one-line purpose shown in the skill listing in the
  system prompt. **Keep it ≤200 chars** — the listing is for discovery
  only, the model loads full content on activation.

**Highly recommended:**

- `when_to_use` — guidance for the model on when to auto-activate. Start
  with "Use when…" and include 1-2 example trigger phrases. **This drives
  auto-activation accuracy.**
- `tags` — searchable tags; helps users find the skill via `/skills`
- `allowed_tools` — security/scoping. Empty = all tools (rare). Most
  skills should explicitly list only what they need.

**Optional:**

- `argument_hint` — usage hint for skills that take parameters,
  e.g. `"<file_path>"` or `"[issue description]"`
- `user_invocable` — defaults to `true`. Set `false` for model-only
  skills (hidden from `/skills` but still activatable by the model).

### 5. Anatomy of the body

Mirror the structure of bundled skills:

- **Title** — `# Skill Name` (markdown H1)
- **Role + goal** — one or two sentences
- **Principles** — 3-5 bullets of key behavioral constraints
- **Process** — numbered steps; each step concrete and actionable
- **Anti-patterns / gotchas** — what NOT to do, with brief reasoning

Optionally add:

- **Reference example** — point at a real file the model can `Read`
- **Output format** — if the skill produces structured output

### 6. Write and confirm

1. Use `Write` to create the file at `<chosen-path>/<name>/SKILL.md`.
   Note: koda needs the **directory** to exist; the `Write` tool will
   create parent directories if needed.
2. Tell the user:
   - Where the file was saved
   - How to invoke it: `/<name>` or via `ActivateSkill("<name>")`
   - That they can edit the SKILL.md directly to refine it

## Anti-patterns to avoid

- **Don't omit `when_to_use`.** Without it, the model has no signal for
  when to auto-activate. The skill becomes user-invoke-only by accident.
- **Don't grant `Write`/`Edit`/`Delete` unless the skill actually modifies
  files.** A code-review skill with `Write` access is a footgun.
- **Don't write a 500-line SKILL.md for a 5-step workflow.** Concision is
  a public good — every skill activation pays the token cost.
- **Don't duplicate context the model already has.** Don't explain what
  `Grep` does. Don't list every git command. Skills add value by
  encoding *workflow* and *constraints*, not basic knowledge.
- **Don't conflate creating a skill with capturing a session.** This
  skill scaffolds a new SKILL.md from a description. If the user wants
  to capture an in-progress workflow, that's a different (future)
  skillify-style operation.

## Reference

For a battle-tested example, read `koda-core/skills/code-review/SKILL.md`
or `koda-core/skills/debug/SKILL.md`. They demonstrate: tight `when_to_use`
phrasing, scoped `allowed_tools`, and concise process sections.
