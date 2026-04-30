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
| `disallowed_tools` | | Tools to deny even if `allowed_tools` is empty |
| `max_iterations` | | Per-sub-agent turn cap (default: **30** for sub-agents, 200 for top-level). See [Sub-agent budget](#sub-agent-budget). |
| `skip_memory` | | Skip injecting project/global memory into the prompt (saves tokens for read-only agents) |

## Sub-agent budget

Sub-agents are bounded by a per-invocation turn cap to prevent
runaway exploration (#1135). The default is **30 turns** —
empirically enough for any reasonable read-only investigation on a
moderate codebase, and matching `gemini-cli`'s `DEFAULT_MAX_TURNS`.
Long-running write agents that legitimately need more can opt up:

```json
{
  "name": "big-refactor",
  "max_iterations": 100,
  "...": "..."
}
```

When a sub-agent hits its budget without producing a final answer,
Koda runs **one grace turn** with this system reminder appended:

> You have reached the maximum number of turns. You have ONE final
> chance to complete the task. You MUST respond with your best
> answer NOW as plain text. DO NOT call any more tools — any tool
> calls in this response will be ignored.

If the model complies, its grace-turn text becomes the sub-agent's
result. If the model defies the reminder and emits more tool calls,
those calls are dropped and the sub-agent returns a `[max_turns
reached: ...]` marker so the parent (and user) can see what
happened.

The top-level Koda agent still uses the larger 200-turn cap with
an interactive extension prompt (`LoopCapReached`) — the budget
pattern is sub-agent-only.

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
