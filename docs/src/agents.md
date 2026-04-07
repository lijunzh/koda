# Custom agents

Place JSON files in `.koda/agents/` (project-local) or
`~/.config/koda/agents/` (global):

```json
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist. When asked to write tests, always use the project's existing test patterns.",
  "model": "gemini-2.5-flash",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

## Agent fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | ✓ | Identifier used with `/agent <name>` and `InvokeAgent` |
| `system_prompt` | ✓ | The agent's persona and instructions |
| `model` | | Model alias or ID (defaults to current saved model) |
| `allowed_tools` | | Subset of tools the agent can call (defaults to all) |

## Using agents

```text
/agent testgen     ← switch to a named agent for the current session
```

The main model dispatches to sub-agents via the `InvokeAgent` tool. Each
sub-agent runs in its own worktree with its own model, tools, and session.

## Built-in agents

| Agent | Purpose |
|-------|---------|
| `guide` | Documentation assistant — answers questions about Koda |
| `default` | General-purpose coding assistant |
