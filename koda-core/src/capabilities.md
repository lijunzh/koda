## Koda Quick Reference

Refer to this when the user asks "what can you do?" or about features.

### Commands (user types these in the REPL)

/agent — list sub-agents | /compact — reclaim context
/diff — git diff/review/commit | /exit — quit | /expand — show full tool output
/memory — persistent memory | /model — switch model
/provider — switch provider | /purge — delete archived history
/sessions — resume/delete sessions | /skills — list skills
/undo — undo last turn | /verbose — toggle full tool output
Shift+Tab — cycle approval mode (auto/confirm)

### Input

- `@file.rs` attaches file context, `@image.png` for multi-modal analysis
- Piped input: `echo "explain" | koda` or `koda -p "prompt"` for headless/CI

### Approval

Two modes (cycle with Shift+Tab): **auto** (default), **confirm**.
Hotkeys during tool confirmation: `y` approve, `n` reject, `f` feedback, `a` always.

### Git Checkpointing

Auto-snapshots working tree before each turn. `/undo` to rollback.

### Skills

Expert instruction modules — zero cost, instant activation via `ActivateSkill`.
- **Built-in:** `code-review` (bugs, anti-patterns), `security-audit` (OWASP checklist)
- **Custom:** `.koda/skills/<name>/SKILL.md` (project) or `~/.config/koda/skills/<name>/SKILL.md` (global)
- Use `ListSkills` to browse, `ActivateSkill` to load expert guidance into context.
- `/skills` lists all available skills from the REPL.

### Agents

- **default** — built-in general-purpose agent
- Custom agents: JSON files in `agents/` or `~/.config/koda/agents/`

### Memory

- Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | Global: `~/.config/koda/memory.md`

### First-Party Libraries

Direct library integrations (no IPC): `koda-ast` (AST analysis), `koda-email` (email via IMAP/SMTP).
