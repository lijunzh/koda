# Tools reference

Koda exposes these tools to the model. In **Safe** trust mode you'll
be prompted before each mutating call. In **Auto** mode, all actions
within the project sandbox are auto-approved.

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
| `TodoRead` | Read-only | Read the session task list |
| `TodoWrite` | Mutating | Update the session task list |
| `RecallContext` | Read-only | Search session history for past context |
| `ListSkills` | Read-only | List available skills |
| `ActivateSkill` | Internal | Load a skill's instructions into context |
| `InvokeAgent` | Varies | Delegate a task to a named sub-agent |
| `ListFiles` | Read-only | List directory contents |
| `AskUser` | Interactive | Ask the user a clarifying question |
| `ListBackgroundTasks` | Read-only | Snapshot all background tasks owned by the caller |
| `CancelTask` | Read-only | Cancel a background agent or shell process |
| `WaitTask` | Read-only | Block until a background task finishes (max 300 s) |

## Approval behaviour by trust mode

| Category | Tools | Plan | Safe | Auto |
|----------|-------|------|------|------|
| Read-only | Read, Grep, Glob, ListFiles, WebFetch, WebSearch, TodoRead, RecallContext | ✅ Auto | ✅ Auto | ✅ Auto |
| Internal | Think, ActivateSkill | ✅ Auto | ✅ Auto | ✅ Auto |
| Mutations | Write, Edit, MemoryWrite, TodoWrite | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Destructive | Delete | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Agent calls | InvokeAgent | ✅ Auto | ✅ Auto | ✅ Auto |
| Background tasks | ListBackgroundTasks, CancelTask, WaitTask | ✅ Auto | ✅ Auto | ✅ Auto |
| User interaction | AskUser | ⏸ Prompt | ⏸ Prompt | ⏸ Prompt |
| Safe shell | `git status`, `grep`, `cargo test` | ✅ Auto | ✅ Auto | ✅ Auto |
| Mutating shell | `echo > file`, `gh issue create` | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Destructive shell | `rm -rf`, `sudo`, `git push --force` | ❌ Deny | ⏸ Prompt | ✅ Auto |
| Outside-project write | Write/Edit to paths outside project root | ❌ Deny | ⏸ Prompt | ⏸ Prompt |

See [Trust modes](./approval.md) for the canonical policy matrix.
