# Skills

Skills are reusable expertise modules — markdown files loaded into the
system prompt on demand.

```text
/skills                  ← list all available skills
/skills security         ← filter by name or description
```

The model can also activate skills automatically via the `ActivateSkill`
tool when it determines a skill is relevant.

## Creating custom skills

Place a `SKILL.md` file inside a named directory under `.koda/skills/`
(project-local) or `~/.config/koda/skills/` (global). The directory name
becomes the skill name.

```text
.koda/
  skills/
    my-checklist/
      SKILL.md        ← skill content goes here
```

### SKILL.md format

Frontmatter is YAML between `---` fences; the body is the skill prompt:

```markdown
---
name: my-checklist
description: One-line summary shown in ListSkills output.
tags: [review, quality]
when_to_use: Use when the user asks to review a pull request or a diff.
allowed_tools: [Read, Grep, Glob]
user_invocable: true
argument_hint: <file_or_pr_url>
---

# My Review Checklist

When reviewing code, always check:
- [ ] No hardcoded secrets
- [ ] Error handling covers all paths
- [ ] Tests cover the new logic
```

| Field | Required | Description |
|---|---|---|
| `name` | ✔ | Skill identifier (used with `ActivateSkill`) |
| `description` | recommended | One-line summary shown in `ListSkills` |
| `tags` | optional | Searchable tags: `[tag1, tag2]` |
| `when_to_use` | recommended | Guidance for the model on when to activate |
| `allowed_tools` | optional | Restrict tools during activation (empty = all) |
| `user_invocable` | optional | `false` = model-only, hidden from `/skills` |
| `argument_hint` | optional | Usage hint (e.g. `<file_path>`) |

> **Note:** Both underscore (`allowed_tools`) and hyphenated (`allowed-tools`)
> field names are accepted for CC compatibility.

### How `allowed_tools` enforcement works

When a skill with `allowed_tools` is activated:

1. **Tool definitions are filtered** — only the listed tools (plus meta-tools
   like `ActivateSkill`, `ListSkills`, `ListAgents`, `InvokeAgent`, `AskUser`)
   are sent to the LLM on subsequent turns.
2. **Blocked tool calls are rejected** — if the model still attempts a blocked
   tool (e.g., from cached context), the call returns an error explaining the
   scope restriction.
3. **Scope clears automatically** when a different skill without `allowed_tools`
   is activated.

Scope transitions are logged as info events (`🔒 scope activated` /
`🔓 scope cleared`).

## Skill lookup order

1. `.koda/skills/` (project-local, highest priority)
2. `~/.config/koda/skills/` (global)
3. Built-in skills bundled with the binary

Project-local skills shadow global ones with the same name.
