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

## Approval behaviour by trust mode

| Category | Tools | Auto | Safe |
|----------|-------|------|------|
| Read-only | Read, Grep, Glob, ListFiles, WebFetch, WebSearch, TodoRead, RecallContext | ✅ Auto | ✅ Auto |
| Internal | Think, ActivateSkill | ✅ Auto | ✅ Auto |
| Mutations | Write, Edit, MemoryWrite, TodoWrite | ✅ Auto | ⏸ Prompt |
| Destructive | Delete | ✅ Auto | ⏸ Prompt |
| Agent calls | InvokeAgent | ✅ Auto | ✅ Auto |
| User interaction | AskUser | ⏸ Prompt | ⏸ Prompt |
| Safe shell | `git status`, `grep`, `cargo test` | ✅ Auto | ✅ Auto |
| Mutating shell | `echo > file`, `gh issue create` | ✅ Auto | ⏸ Prompt |
| Destructive shell | `rm -rf`, `sudo`, `git push --force` | ✅ Auto | ⏸ Prompt |
| Outside-project write | Write/Edit to paths outside project root | ⏸ Prompt | ⏸ Prompt |
