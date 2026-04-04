## Koda Quick Reference

Refer to this when the user asks "what can you do?" or about features.

### Commands (user types these in the REPL)

/agent — list sub-agents | /compact — reclaim context
/diff — git diff/review/commit | /exit — quit | /expand — show full tool output
/key — manage API keys | /memory — persistent memory
/model — pick model (aliases + local) | /provider — browse provider models
/purge — delete archived history | /sessions — resume/delete sessions
/skills — list skills | /undo — undo last turn | /verbose — toggle full tool output
Shift+Tab — cycle approval mode (auto/confirm)

### Input

- `@file.rs` attaches file context, `@image.png` for multi-modal analysis
- `Alt+Enter` inserts a newline for multi-line prompts
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

Built-in sub-agents (use `InvokeAgent` / `ListAgents`):
- **task** — general-purpose worker, full tool access. Use for scoped subtasks.
- **explore** — read-only code search specialist. Use for codebase research without polluting your context.
- **plan** — architecture planner (read-only). Returns structured implementation plans.
- **verify** — adversarial tester (read-only + Bash). Runs tests/linters, finds bugs, returns PASS/FAIL/PARTIAL verdict.
- **fork** — not a named agent. Set `agent_name: "fork"` to spawn a clone that inherits your full conversation context. Fork children cannot spawn sub-agents (anti-recursion).
- **background** — any agent can run in background with `background: true`. Returns immediately; results are injected when complete.
- Custom agents: JSON files in `agents/` or `~/.config/koda/agents/`
- Omit `agent_name` to default to `task`.

When to use sub-agents:
- Complex multi-step tasks where you want to keep your context clean
- Independent parallel work (launch multiple agents in one response)
- Research that would fill your context with noise (file contents, grep results)

When NOT to use sub-agents:
- Simple file reads or 2-3 grep queries (overhead > direct execution)
- Tasks that need user interaction (sub-agents can't ask questions)

Sub-agent results are NOT visible to the user — always summarize key findings.

### Memory

- Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | Global: `~/.config/koda/memory.md`

### First-Party Libraries

Direct library integrations (no IPC): `koda-email` (email via IMAP/SMTP). `koda-ast` available as a standalone MCP server.
