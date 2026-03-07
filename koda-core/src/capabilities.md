## Koda Quick Reference

Refer to this when the user asks "what can you do?" or about features.

### Commands (user types these in the REPL)

/help — command palette | /agent — list sub-agents | /compact — reclaim context
/cost — token usage & cost | /diff — git diff/review/commit | /expand — show full tool output
/mcp — MCP server management | /memory — persistent memory | /model — switch model
/provider — switch provider | /sessions — resume/delete sessions (interactive picker)
/trust — plan/normal/yolo | /verbose — toggle full tool output | /exit — quit

### Input

- `@file.rs` attaches file context, `@image.png` for multi-modal analysis
- Piped input: `echo "explain" | koda` or `koda -p "prompt"` for headless/CI

### Model Tiers

Koda auto-adapts to your model. Override with `--model-tier strong|standard|lite`.
- Strong (Opus, GPT-4o): minimal prompts, lazy tools, parallel execution
- Standard (Flash, Mistral): full prompts, all tools
- Lite (local models): verbose prompts, step-by-step, sequential

### Built-in Agents

- **scout** — read-only codebase exploration
- **testgen** — test generation
- **planner** — task decomposition
- **verifier** — quality verification
- **releaser** — release engineering

### Memory

- Project: `MEMORY.md` (also reads `CLAUDE.md`, `AGENTS.md`) | Global: `~/.config/koda/memory.md`

### MCP

External tool servers configured in `.mcp.json` (project) or `~/.config/koda/mcp.json` (global).
MCP tools appear with namespaced names like `github.create_issue`.
Auto-provisioned: `koda-ast` (AST analysis), `koda-email` (email via IMAP/SMTP).
