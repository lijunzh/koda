# Koda User Guide

Workflow-oriented documentation for getting things done with Koda.
For architecture and design decisions, see [DESIGN.md](../DESIGN.md).
For developer docs (building, testing, contributing), see [CLAUDE.md](../CLAUDE.md).

## Table of Contents

- [Quick Start](#quick-start)
- [Approval Modes](#approval-modes)
- [Slash Commands](#slash-commands)
- [File References](#file-references)
- [Memory System](#memory-system)
- [Agents](#agents)
- [MCP Servers](#mcp-servers)
- [Git Checkpointing](#git-checkpointing)
- [Headless Mode](#headless-mode)
- [Security Model](#security-model)

---

## Quick Start

```bash
# Interactive session (default)
koda

# One-shot prompt (headless mode for scripts/CI)
koda -p "explain this codebase"

# Resume a previous session
koda -s <session-id>
```

On first launch, Koda runs a provider setup wizard. After that, it
drops you into an interactive REPL with streaming LLM output and
inline tool execution.

---

## Approval Modes

Koda gates tool execution with three approval modes. Cycle with **Shift+Tab**.

| Mode | What's auto-approved | What needs confirmation | What's blocked |
|------|---------------------|----------------------|---------------|
| **Auto** (default) | Reads, remote actions, local writes inside project | Destructive ops (delete, `rm -rf`, force push), writes outside project | Nothing |
| **Strict** | Reads, remote actions | All local writes and destructive ops | Nothing |
| **Safe** | Reads, remote actions | — | All local filesystem mutations |

**Hardcoded safety floors** (apply in every mode):
- Writes outside the project root always require confirmation
- Destructive bash commands (`rm -rf`, `git push --force`) always require confirmation
- Bash commands that escape the project (`cd /tmp`, absolute paths outside project) always require confirmation

**Approval hotkeys** (shown inline when a tool needs confirmation):
- `y` — approve
- `n` — reject
- `f` — reject with feedback (type a reason, the model adapts)
- `a` — approve and switch to Auto mode

---

## Slash Commands

Type `/` to open the command palette with descriptions. Tab to complete.

| Command | Description |
|---------|-------------|
| `/agent` | List available sub-agents |
| `/compact` | Summarize conversation to reclaim context |
| `/cost` | Show token usage and cost for this session |
| `/diff` | Show git diff, review changes, or commit |
| `/exit` | Quit the session |
| `/expand` | Show full output of last collapsed tool call |
| `/mcp` | MCP server management (status, add, remove) |
| `/memory` | View or save project and global memory |
| `/model` | Pick a model interactively |
| `/provider` | Switch LLM provider |
| `/sessions` | List, resume, or delete sessions |
| `/skills` | List available skills (search with `/skills <query>`) |
| `/undo` | Undo last turn's file changes |
| `/verbose` | Toggle full tool output display |

**Power-user shortcuts**: Commands accept inline arguments to skip wizards.
For example, `/provider anthropic` switches instantly if an API key is
already stored, or starts the wizard at the API key step if not.

---

## File References

Attach files to your prompt with `@`:

```
@src/main.rs explain this entry point
@screenshot.png what's wrong with this UI?
```

**Text files** are injected as tagged context blocks. **Images** (png, jpg,
gif, webp, bmp) are base64-encoded for multi-modal analysis.

You can also drag-and-drop image paths from your file manager — bare
absolute paths like `/Users/me/Desktop/screenshot.png` are auto-detected.

Security: paths that escape the project root via `../` are rejected.

---

## Memory System

Koda maintains persistent memory at two levels:

| Level | File | Purpose |
|-------|------|---------|
| **Project** | `MEMORY.md` in project root | Project-specific conventions, patterns, decisions |
| **Global** | `~/.config/koda/memory.md` | Cross-project preferences, coding style |

**Compatibility**: Koda also reads `CLAUDE.md` and `AGENTS.md` if `MEMORY.md`
doesn't exist (fallback chain: `MEMORY.md` → `CLAUDE.md` → `AGENTS.md`).
Writes always go to `MEMORY.md`.

Both tiers are injected into the system prompt at session start. Use
`/memory` to view or manually edit memory.

**Recommended**: Put project rules in `CLAUDE.md` (works with both Koda and
Claude Code). See [DESIGN.md §13](../DESIGN.md) for rationale.

---

## Agents

Koda ships with one built-in agent (`default`). You can create custom agents
for specialized workflows.

### Creating a custom agent

Create a JSON file in `agents/` (project-local) or `~/.config/koda/agents/`
(user-global):

```json
{
  "name": "reviewer",
  "system_prompt": "You are a code reviewer. Focus on correctness, performance, and readability.",
  "allowed_tools": ["Read", "Grep", "Glob"],
  "model": "claude-sonnet-4-20250514",
  "provider": "anthropic",
  "max_iterations": 20
}
```

**Key fields**:
- `name` — identifier (used with `-a reviewer` or `/agent`)
- `system_prompt` — replaces the default prompt
- `allowed_tools` — restrict which tools the agent can use (empty `[]` = all tools)
- `model`, `provider`, `base_url` — override the parent session's LLM
- `max_tokens`, `temperature`, `thinking_budget`, `reasoning_effort` — model tuning
- `max_iterations` — hard cap on tool-call loops
- `auto_compact_threshold` — context % at which auto-compaction triggers

**Search order**: project `agents/` → user `~/.config/koda/agents/` → built-in.
First match wins.

**Sub-agents**: The model can invoke agents via `InvokeAgent` tool calls.
Sub-agents inherit the parent's approval mode (clamped — a Safe parent
produces a Safe child). See [DESIGN.md §18](../DESIGN.md) for delegation scoping.

---

## MCP Servers

MCP (Model Context Protocol) servers extend Koda with external tool
capabilities — AST analysis, email, databases, GitHub, and anything
with an MCP adapter.

### Configuration

Create `.mcp.json` in your project root (or `~/.config/koda/mcp.json` for global):

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "env": { "NODE_ENV": "production" },
      "timeout": 30
    }
  },
  "toolOverrides": {
    "filesystem.write_file": "Destructive"
  }
}
```

**Server fields**:
- `command` (required) — executable to launch (e.g. `npx`, `uvx`, `python`)
- `args` — command-line arguments
- `env` — environment variables (supports `$VAR` and `${VAR}` expansion)
- `timeout` — connection timeout in seconds (default: 30)

**Tool effect overrides**: Override the safety classification of specific
MCP tools. Values: `ReadOnly`, `LocalMutation`, `RemoteAction`, `Destructive`.

### Auto-provisioned servers

Koda ships two MCP servers that auto-install on first use:
- **koda-ast** — tree-sitter AST analysis (Rust, Python, JS, TS, Go, Java, C, C++)
- **koda-email** — email via IMAP/SMTP (requires `KODA_EMAIL_*` env vars)

### Management

Use `/mcp` to view status, add, or remove servers interactively.

---

## Git Checkpointing

Koda automatically snapshots your working tree before each inference turn
using `git stash create` (non-destructive — doesn't modify your working tree
or stash list).

- **Undo last turn**: `/undo` restores files to the pre-turn state
- **Rollback**: The model can call the `Undo` tool to rollback programmatically
- **Requirement**: Must be in a git repository (checkpointing silently skips otherwise)

The snapshot is lightweight — it records dirty state without creating a
commit or modifying HEAD.

---

## Headless Mode

Run Koda non-interactively for scripts, CI pipelines, and automation:

```bash
# Basic usage
koda -p "fix the failing test in src/lib.rs"

# JSON output for parsing
koda -p "list all TODO comments" --output-format json

# Read prompt from stdin
echo "explain this code" | koda -p -

# Override provider and model
koda -p "review changes" --provider anthropic --model claude-sonnet-4-20250514

# Resume a session
koda -p "continue" -s <session-id>
```

**Output formats**:
- `text` (default) — plain text to stdout
- `json` — structured response: `{ "success": bool, "response": string, "session_id": string, "model": string }`

**Behavior**: Headless mode uses Auto approval (all tool calls auto-approved).
The model probe runs once per model (cached in `~/.config/koda/model_probes.json`).

**CLI flags**: `--max-tokens`, `--temperature`, `--thinking-budget`,
`--reasoning-effort`, `--skip-probe`, `--project-root`.

---

## Security Model

Koda treats the LLM as semi-trusted — capable of mistakes but not adversarial.
The security model focuses on preventing accidental blast radius.

### Defense layers

1. **Tool effect classification** — every tool call is tagged as ReadOnly,
   LocalMutation, Destructive, or RemoteAction
2. **Approval modes** — Auto/Strict/Safe control which effects need confirmation
3. **Folder scoping** — writes outside project root always need confirmation
4. **Bash linting** — heuristic analysis catches `cd` escapes, absolute paths
   outside project, and dangerous patterns (`rm -rf`, force push)
5. **Sub-agent delegation** — children inherit clamped permissions from parent

### What Koda does NOT protect against

- Kernel-level sandboxing (no seccomp/landlock) — planned for v1.0
- Complex shell pipelines that evade heuristic parsing
- Malicious MCP servers lying about `readOnly` annotations
- Auto mode sub-agents with unconstrained `FullProject` scope

For the full security analysis, see [DESIGN.md §18](../DESIGN.md).
