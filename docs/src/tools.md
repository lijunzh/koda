# Tools reference

Koda exposes these tools to the model. In **Confirm** approval mode you'll
be prompted before each mutating call. In **Auto** mode, only destructive
Bash commands require confirmation.

| Tool | Effect | Description |
|------|--------|-------------|
| `Read` | Read-only | Read a file (with optional line range) |
| `Write` | Mutating | Create or overwrite a file |
| `Edit` | Mutating | Targeted text replacement within a file |
| `Delete` | Mutating | Delete a file or directory |
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

## Approval behaviour by tool

| Category | Tools | Auto mode | Confirm mode |
|----------|-------|-----------|--------------|
| Read-only | Read, Grep, Glob, ListFiles, WebFetch, WebSearch, TodoRead, RecallContext | ✅ Auto | ✅ Auto |
| Internal | Think, ActivateSkill | ✅ Auto | ✅ Auto |
| Safe writes | Write, Edit, Delete, MemoryWrite, TodoWrite | ✅ Auto | ⏸ Prompt |
| Agent calls | InvokeAgent | ✅ Auto | ⏸ Prompt |
| User interaction | AskUser | ⏸ Prompt | ⏸ Prompt |
| Destructive shell | `rm -rf`, `sudo`, `git push --force`, … | ⏸ Prompt | ⏸ Prompt |
