# Tools reference

Koda exposes these tools to the model. In **Safe** trust mode you'll
be prompted before each mutating call. In **Auto** mode, mutating ops
within the project sandbox auto-approve, but **destructive** ops still
prompt. See [Trust modes](./approval.md) for the canonical matrix.

| Tool | Effect | Description |
|------|--------|-------------|
| `Read` | Read-only | Read a file (with optional line range) |
| `Write` | Mutating | Create or overwrite a file |
| `Edit` | Mutating | Targeted text replacement within a file |
| `Delete` | Destructive | Delete a file or directory |
| `Bash` | Varies | Run a shell command |
| `Grep` | Read-only | Search for patterns across files (ripgrep) |
| `Glob` | Read-only | List files matching a glob pattern |
| `WebFetch` | Read-only | Fetch a URL and return its text content |
| `WebSearch` | Read-only | Search the web via DuckDuckGo |
| `Think` | Internal | Extended reasoning step (no side effects) |
| `MemoryRead` | Read-only | Read from project or global memory |
| `MemoryWrite` | Mutating | Append a fact to a memory file |
| `TodoWrite` | Read-only | Update the session task list (Koda-owned state, no FS impact) |
| `RecallContext` | Read-only | Search session history for past context |
| `ListSkills` | Read-only | List available skills |
| `ActivateSkill` | Internal | Load a skill's instructions into context |
| `InvokeAgent` | Varies | Delegate a task to a named sub-agent |
| `ListFiles` | Read-only | List directory contents |
| `AskUser` | Interactive | Ask the user a clarifying question |
| `ListBackgroundTasks` | Read-only | Snapshot all background tasks owned by the caller |
| `CancelTask` | Read-only | Cancel a background agent or shell process |
| `WaitTask` | Read-only | Block until one or more background tasks finish (max 300 s/task, atomic gather) |

## Approval behaviour by trust mode

| Category | Tools | Plan | Safe | Auto |
|----------|-------|------|------|------|
| Read-only | Read, Grep, Glob, ListFiles, WebFetch, WebSearch, RecallContext, TodoWrite | ✅ Auto | ✅ Auto | ✅ Auto |
| Internal | Think, ActivateSkill | ✅ Auto | ✅ Auto | ✅ Auto |
| Mutations | Write, Edit, MemoryWrite | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Destructive | Delete | ❌ Deny | ⏸ Prompt | ⏸ Prompt |
| Agent calls | InvokeAgent | ✅ Auto | ✅ Auto | ✅ Auto |
| Background tasks | ListBackgroundTasks, CancelTask, WaitTask | ✅ Auto | ✅ Auto | ✅ Auto |
| User interaction | AskUser | ⏸ Prompt | ⏸ Prompt | ⏸ Prompt |
| Safe shell | `git status`, `grep`, `cargo test` | ✅ Auto | ✅ Auto | ✅ Auto |
| Mutating shell | `echo > file`, `gh issue create` | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Destructive shell | `rm -rf`, `sudo`, `git push --force`, `git reset --hard` | ❌ Deny | ⏸ Prompt | ⏸ Prompt |
| Outside-project write | Write/Edit to paths outside project root | ❌ Deny | ⏸ Prompt | ⏸ Prompt |

See [Trust modes](./approval.md) for the canonical policy matrix and
the sub-agent context-sensitive variant (sub-agents resolve `⏸ Prompt`
as auto-approve for mutations and block for destructive ops, since
there's no human channel to prompt into).

## WaitTask

`WaitTask` blocks until one or more background tasks (sub-agents or
shell processes) finish. As of v0.2.25 it is an **atomic multi-task
gather**: pass an array of task IDs and get back results for all of
them in a single tool call, with per-task error isolation.

### Request shape

```json
{
  "task_ids": ["agent:1", "agent:2", "sh:7"]
}
```

- `task_ids` (required): array of strings. Each ID comes from the
  "started" message printed by `InvokeAgent` (e.g. `agent:1`) or by
  `Bash` when run in the background (e.g. `sh:7`).
- One ID per task; duplicates are deduplicated.
- Per-task budget is **300 seconds** (5 minutes). The budget is
  per-task, not per-call: gathering 4 tasks does not give you 1200 s.

### Response shape

```json
{
  "tasks": [
    {"task_id": "agent:1", "status": "success", "result": {...}},
    {"task_id": "agent:2", "status": "failed",  "error": "..."},
    {"task_id": "sh:7",     "status": "timeout"}
  ],
  "summary": {
    "total": 3, "success": 1, "failed": 1, "timeout": 1,
    "cancelled": 0, "forbidden": 0, "not_found": 0
  }
}
```

Per-task `status` is one of: `success`, `failed`, `timeout`,
`cancelled`, `forbidden` (caller does not own the task), or
`not_found` (typo or already-reaped task).

### Error isolation

A failure in one task does **not** fail the whole call. The model can
inspect the `summary` block to decide what to retry. This is the
breaking change from v0.2.24, where `WaitTask` accepted a single
`task_id` and returned the bare result envelope.

### Migration from v0.2.24

Any prompt or skill that hardcoded `{"task_id": "agent:1"}` must be
updated to `{"task_ids": ["agent:1"]}`. Single-task waits still work
— the array just has length 1.
