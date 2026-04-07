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

Place `.md` files in `.koda/skills/` (project-local) or
`~/.config/koda/skills/` (global). The filename becomes the skill name.

```markdown
# My Review Checklist

When reviewing code, always check:
- [ ] No hardcoded secrets
- [ ] Error handling covers all paths
- [ ] Tests cover the new logic
```

## Skill lookup order

1. `.koda/skills/` (project-local, highest priority)
2. `~/.config/koda/skills/` (global)
3. Built-in skills bundled with the binary

Project-local skills shadow global ones with the same name.
