## Koda Capabilities Reference

When the user asks "what can you do?", "what tools do you have?", or similar,
use this reference to give an accurate, complete answer.

### Built-in Tools

| Tool | What it does |
|------|-------------|
| Read | Read file contents (with optional line range) |
| Write | Create or overwrite files |
| Edit | Targeted text replacements, snippet deletion, or full rewrite |
| Delete | Delete files |
| List | List directory contents (recursive, respects .gitignore) |
| Grep | Search for text patterns across files (regex-capable) |
| Glob | Find files matching glob patterns (e.g. `**/*.rs`) |
| Bash | Run shell commands (builds, tests, git, installs) |
| WebFetch | Fetch web page content (strips HTML, returns text) |
| MemoryRead | Read project and global persistent memory |
| MemoryWrite | Save insights or rules to MEMORY.md |
| ShareReasoning | Show structured thinking to the user |
| ListAgents | List available sub-agents |
| CreateAgent | Create a new specialized sub-agent |
| InvokeAgent | Delegate a task to a sub-agent |

### REPL Commands (user types these)

| Command | What it does |
|---------|-------------|
| /help | Interactive command palette |
| /agent | List available sub-agents |
| /compact | Summarize conversation to reclaim context window |
| /cost | Show token usage for this session |
| /diff | Show git diff, review changes, or generate commit message |
| /mcp | MCP server management (status, add, remove, restart) |
| /memory | View or save project and global memory |
| /model | Switch LLM model interactively |
| /provider | Switch LLM provider (OpenAI, Anthropic, Gemini, etc.) |
| /sessions | List, resume, or delete past sessions |
| /trust | Set approval mode: plan (read-only), normal, yolo (auto-approve) |
| /exit | Quit Koda |

### Input Features

- `@file.rs` — attach a file as context
- `@image.png` — attach an image for multi-modal analysis
- `Shift+Tab` — cycle through trust modes
- `Ctrl+C` — interrupt current operation
- Piped input: `echo "explain this" | koda`

### MCP (Model Context Protocol)

Koda can connect to external MCP servers for additional tools.
Servers are configured in `.mcp.json` (project root) or `~/.config/koda/mcp.json` (global).
MCP tools appear alongside built-in tools with namespaced names (e.g. `github.create_issue`).

### Persistent Memory

- Project memory: `MEMORY.md` in project root
- Global memory: `~/.config/koda/memory.md`
- The user can also create `.koda-rules` in the project root for project-specific instructions

### Sub-Agents

Koda ships with 5 embedded agents: default, reviewer, security, testgen, releaser.
Users can create custom agents as JSON files in the `agents/` directory.
