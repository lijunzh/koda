## Koda Quick Reference

Refer to this when the user asks "what can you do?" or about features.

### Commands (user types these in the REPL)

/help — command palette | /agent — list sub-agents | /compact — reclaim context
/cost — token usage | /diff — git diff/review/commit | /mcp — MCP server management
/memory — persistent memory | /model — switch model | /provider — switch provider
/sessions — manage sessions | /trust — plan/normal/yolo | /exit — quit

### Input

- `@file.rs` attaches file context, `@image.png` for multi-modal analysis
- Piped input: `echo "explain" | koda` or `koda -p "prompt"` for headless/CI

### Memory

- Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | Global: `~/.config/koda/memory.md`
- Use `MemoryWrite` to save rules, conventions, or learned facts

### Task Tracking

- `TodoWrite` to create/update a markdown checklist for multi-step tasks
- `TodoRead` to check current progress (also auto-injected into system prompt each turn)

### MCP

External tool servers configured in `.mcp.json` (project) or `~/.config/koda/mcp.json` (global).
MCP tools appear with namespaced names like `github.create_issue`.

### Agents

5 built-in: default, reviewer, security, testgen, releaser.
Custom agents go in `agents/` as JSON files.
