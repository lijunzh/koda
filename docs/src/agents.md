# Custom agents

Place JSON files in `.koda/agents/` (project-local) or
`~/.config/koda/agents/` (global; `$XDG_CONFIG_HOME/koda/agents/` when that's
set). A project's agent overrides a global one of the same name, which
overrides a built-in one. Earlier releases read project agents from
`agents/` at the project root, despite these docs; that folder is still
read for one more release, with a warning in the log, so move its files to
`.koda/agents/`.

```json
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist. When asked to write tests, always use the project's existing test patterns.",
  "model": "gemini-2.5-flash",
  "trust": "auto",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

## Agent fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | ✓ | Identifier used with `--agent <name>` and `InvokeAgent` |
| `system_prompt` | ✓ | The agent's persona and instructions |
| `trust` | | `"auto"` (write-capable: edits and commands run in the sandbox) or `"plan"` (read-only). An agent that names none runs in Auto, without `Write` and `Edit` unless it has the legacy `write_access: true` (see [Migration](#migration-from-write_access)). The session's mode caps it: in a Plan session every agent is read-only. Any other value, `"safe"` included (removed in 0.8.0), loads as `"plan"` with a warning in the log. See [Trust modes](./approval.md). |
| `isolation` | | `"worktree"` to work in a git worktree of its own and leave the work on a branch. It needs a git repository: without one, the agent doesn't start. By default it writes in the project, as the main agent does. See [Where sub-agents write](sandbox.md#where-sub-agents-write). |
| `model` | | Model alias or ID (defaults to current saved model) |
| `allowed_tools` | | Subset of tools the agent can call (defaults to all) |
| `disallowed_tools` | | Tools to deny even if `allowed_tools` is empty |
| `max_iterations` | | Turn cap (default: **30** for sub-agents, 200 for top-level). See [Sub-agent budget](#sub-agent-budget). |
| `skip_memory` | | Skip injecting project/global memory into the prompt (saves tokens for read-only agents) |
| ~~`write_access`~~ | | **Deprecated** — use `trust` instead. Pre-existing JSONs still work; a warning is logged at load. See [Migration from `write_access`](#migration-from-write_access). |

## Three example shapes

The `trust` field is the primary mechanism. Most custom agents fall
into one of three shapes:

### Read-only investigator (Plan trust)

For agents that should only investigate, never modify state:

```json
{
  "name": "auditor",
  "system_prompt": "You audit code for security issues. Read, grep, and report — never modify files.",
  "trust": "plan",
  "skip_memory": true
}
```

`trust: "plan"` makes the kernel sandbox enforce read-only at the
syscall level — strictly stronger than soft-denying `Write`/`Edit`
in `disallowed_tools`. (Soft-denying still works as a behavioral floor
for tools the matrix can't gate, like `InvokeAgent` or `AskUser`.)

### Write-capable worker (Auto trust)

The default shape for agents that need to make changes:

```json
{
  "name": "testgen",
  "system_prompt": "You write tests. Match the project's existing patterns.",
  "trust": "auto",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

Its calls go through the session's rules: its edits and commands run in the
sandbox, and what would ask the main agent (a destructive command, leaving the
sandbox) asks in the main session, naming the agent; see
[Sub-agents ask in the main session](./approval.md#sub-agents-ask-in-the-main-session).

### Read-only with execution escape valve (Auto trust + denied writes)

For agents that need to run tests / inspect runtime state but should
never modify code:

```json
{
  "name": "tester",
  "system_prompt": "You verify behavior by running tests. Never edit source.",
  "trust": "auto",
  "disallowed_tools": ["Write", "Edit"]
}
```

This is **strictly different** from `trust: "plan"`: the agent can't
call `Write` or `Edit`, but its `Bash` commands run in Auto's sandbox, which
can write the project, so `cargo test` and `pytest` can write their build
output. Plan runs `Bash` too, but with the project read-only.

## Migration from `write_access`

The legacy `write_access: bool` field is deprecated. The mapping:

| Old shape                                      | New shape                                         |
|-----------------------------------------------|---------------------------------------------------|
| `{ "write_access": true }`                     | `{ "trust": "auto" }`                              |
| `{ "write_access": false }`                    | `{ "trust": "plan" }`                              |
| `{ "write_access": false, "disallowed_tools": [...] }` | `{ "trust": "plan", "disallowed_tools": [...] }`   |

Pre-existing JSONs without `trust` still work via a back-compat
default-deny: `write_access: false` (or absent) injects
`Write`/`Edit` into `disallowed_tools` at load. Adding `trust`
explicitly **skips** the legacy default-deny so an explicit
`trust: "auto"` declaration works cleanly without surprise denials.

A deprecation warning is logged at load when both `trust` and
`write_access: true` appear in the same JSON.

## Sub-agent budget

Sub-agents are bounded by a per-invocation turn cap to prevent
runaway exploration. The default is **30 turns** —
empirically enough for any reasonable read-only investigation on a
moderate codebase, and matching `gemini-cli`'s `DEFAULT_MAX_TURNS`.
Long-running write agents that legitimately need more can opt up:

```json
{
  "name": "big-refactor",
  "trust": "auto",
  "max_iterations": 100,
  "...": "..."
}
```

When a sub-agent hits its budget without producing a final answer,
Koda runs **one grace turn** with this reminder appended as a user
message:

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
koda --agent testgen     ← run the session as a named agent
/agents                  ← list the available agents
```

The main model dispatches to sub-agents via the `InvokeAgent` tool. Each
sub-agent gets its own model, tools, and session. Write-capable sub-agents
write in the project unless they ask for a worktree: see [Where sub-agents write](./sandbox.md#where-sub-agents-write).

In a [confidential session](./sessions.md#confidential-sessions) a
sub-agent starts only on a local model or on the session's own provider and
endpoint: one whose file names another provider is refused, and the model
is told why.

## Built-in agents

| Agent     | Trust  | Purpose                                                                                                                |
|-----------|--------|------------------------------------------------------------------------------------------------------------------------|
| `default` | auto   | General-purpose coding assistant — the master agent shape                                                              |
| `task`    | auto   | Generic write-capable sub-agent for `InvokeAgent` dispatch                                                             |
| `explore` | plan   | Read-only investigation sub-agent — code review, dependency tracing, audit trails                                      |
