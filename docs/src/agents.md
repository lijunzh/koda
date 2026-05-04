# Custom agents

Place JSON files in `.koda/agents/` (project-local) or
`~/.config/koda/agents/` (global):

```json
{
  "name": "testgen",
  "system_prompt": "You are a test generation specialist. When asked to write tests, always use the project's existing test patterns.",
  "model": "gemini-2.5-flash",
  "trust": "safe",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

## Agent fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | ✓ | Identifier used with `/agent <name>` and `InvokeAgent` |
| `system_prompt` | ✓ | The agent's persona and instructions |
| `trust` | | `"plan"` (read-only), `"safe"` (write-capable, default), or `"auto"` (auto-approve mutations within sandbox). See [Trust modes](./approval.md). |
| `model` | | Model alias or ID (defaults to current saved model) |
| `allowed_tools` | | Subset of tools the agent can call (defaults to all) |
| `disallowed_tools` | | Tools to deny even if `allowed_tools` is empty |
| `max_iterations` | | Per-sub-agent turn cap (default: **30** for sub-agents, 200 for top-level). See [Sub-agent budget](#sub-agent-budget). |
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
syscall level — strictly stronger than soft-denying `Write`/`Edit`/`Delete`
in `disallowed_tools`. (Soft-denying still works as a behavioral floor
for tools the matrix can't gate, like `InvokeAgent` or `AskUser`.)

### Write-capable worker (Safe trust)

The default shape for agents that need to make changes:

```json
{
  "name": "testgen",
  "system_prompt": "You write tests. Match the project's existing patterns.",
  "trust": "safe",
  "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
}
```

In Safe trust, the [sub-agent matrix](./approval.md#sub-agent-matrix-context-sensitive-resolution)
auto-approves mutating ops (Write/Edit/MemoryWrite) and blocks
destructive ops (`rm -rf`, `git reset --hard`, `Delete`). No human
prompt needed because there's no UI for the sub-agent to prompt into.

### Read-only with execution escape valve (Safe trust + denied writes)

For agents that need to run tests / inspect runtime state but should
never modify code (e.g. the built-in `verify` agent):

```json
{
  "name": "verify",
  "system_prompt": "You verify behavior by running tests. Never edit source.",
  "trust": "safe",
  "disallowed_tools": ["Write", "Edit", "Delete"]
}
```

This is **strictly different** from `trust: "plan"`: the agent can
still call `Bash` (run `cargo test`, `pytest`, etc.) but the trust
matrix's auto-approve for mutating tools is overridden by the soft
denylist. Plan would deny `Bash` outright as a mutation.

## Migration from `write_access`

The legacy `write_access: bool` field is deprecated. The mapping:

| Old shape                                      | New shape                                         |
|-----------------------------------------------|---------------------------------------------------|
| `{ "write_access": true }`                     | `{ "trust": "safe" }`                              |
| `{ "write_access": false }`                    | `{ "trust": "plan" }`                              |
| `{ "write_access": false, "disallowed_tools": [...] }` | `{ "trust": "plan", "disallowed_tools": [...] }`   |

Pre-existing JSONs without `trust` still work via a back-compat
default-deny: `write_access: false` (or absent) injects
`Write`/`Edit`/`Delete` into `disallowed_tools` at load. Adding `trust`
explicitly **skips** the legacy default-deny so an explicit
`trust: "safe"` declaration works cleanly without surprise denials.

A deprecation warning is logged at load when both `trust` and
`write_access: true` appear in the same JSON.

## Sub-agent budget

Sub-agents are bounded by a per-invocation turn cap to prevent
runaway exploration (#1135). The default is **30 turns** —
empirically enough for any reasonable read-only investigation on a
moderate codebase, and matching `gemini-cli`'s `DEFAULT_MAX_TURNS`.
Long-running write agents that legitimately need more can opt up:

```json
{
  "name": "big-refactor",
  "trust": "safe",
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

| Agent     | Trust  | Purpose                                                                                                                |
|-----------|--------|------------------------------------------------------------------------------------------------------------------------|
| `default` | safe   | General-purpose coding assistant — the master agent shape                                                              |
| `task`    | safe   | Generic write-capable sub-agent for `InvokeAgent` dispatch                                                             |
| `explore` | plan   | Read-only investigation sub-agent — code review, dependency tracing, audit trails                                      |
| `plan`    | plan   | Read-only planning sub-agent — proposes changes without making them                                                    |
| `verify`  | safe   | Execution-only sub-agent — runs tests/scripts but `Write`/`Edit`/`Delete` are denied as a behavioral floor             |
