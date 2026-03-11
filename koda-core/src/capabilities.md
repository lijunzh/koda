## Koda Quick Reference

Refer to this when the user asks "what can you do?" or about features.

### Commands (user types these in the REPL)

/help — command palette | /agent — list sub-agents | /compact — reclaim context
/cost — token usage & cost | /diff — git diff/review/commit | /expand — show full tool output
/mcp — MCP server management | /memory — persistent memory | /model — switch model
/provider — switch provider | /sessions — resume/delete sessions (interactive picker)
Shift+Tab — cycle mode (auto/strict/safe) | /verbose — toggle full tool output | /exit — quit

### Input

- `@file.rs` attaches file context, `@image.png` for multi-modal analysis
- Piped input: `echo "explain" | koda` or `koda -p "prompt"` for headless/CI

### Model Probe

On session start, Koda runs a one-time structured output test to verify the model
can produce valid tool calls. Skip with `--skip-probe`.

### Agents

- **default** — built-in general-purpose agent
- Custom agents: create JSON files in `agents/` directory (e.g. testgen, planner, releaser)

### Memory

- Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | Global: `~/.config/koda/memory.md`

### MCP

External tool servers configured in `.mcp.json` (project) or `~/.config/koda/mcp.json` (global).
MCP tools appear with namespaced names like `github.create_issue`.
Auto-provisioned: `koda-ast` (AST analysis), `koda-email` (email via IMAP/SMTP).
