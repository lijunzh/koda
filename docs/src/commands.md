# Slash commands

Type in the TUI input. Tab-completion is available for all commands.

## `/help`

Shows the quick-reference keybinding card inside the TUI.
This docs site is the full reference; `/help` is the in-session reminder.

## `/model [<alias-or-id>]`

Without an argument: opens an interactive picker listing all model aliases
and any locally running models detected via LM Studio or Ollama.

With an argument: switches immediately.

```text
/model gemini-flash        ← switch by alias
/model claude-opus         ← switch by alias
/model local               ← auto-detect from LM Studio
/model gpt-4o              ← literal model ID (no alias needed)
/model llama3.2            ← any model name your provider understands
```

The new model is persisted to the keystore and used for all future
sessions until changed again. See [Providers & model aliases](./providers.md).

## `/provider [<name>]`

Without an argument: opens a two-step picker — choose provider, then
browse and pick one of its available models.

With an argument: jumps straight to that provider's model list.

```text
/provider                  ← open the picker
/provider anthropic        ← go straight to Anthropic models
/provider ollama           ← browse locally running Ollama models
```

## `/key`

Opens the API key manager. Select a provider, then type or paste your key.
Keys are stored in the local SQLite keystore (file mode 0600) and injected
as environment variables at every startup.

Shell env vars always win over stored keys — so `export ANTHROPIC_API_KEY=…`
in your shell or `.envrc` is always a clean override.

## `/compact`

Summarises old conversation history to free context tokens. Koda
auto-compacts when the context window hits **85%** full, but you can
trigger it manually at any time:

- All but the **last 4 messages** are summarised by the model
- The summary replaces the old messages in the DB
- The compressed session continues normally
- Use `/purge` later to clean up the archived messages

## `/purge [<age>]`

Deletes compacted (archived) message history. Does not touch the live messages
in your current session.

```text
/purge        ← delete all archived messages (prompts for confirmation)
/purge 90d    ← only messages archived more than 90 days ago
/purge 30d    ← only messages archived more than 30 days ago
```

Requires `y` to confirm. Deleted messages are gone permanently.

## `/undo`

Reverts all file mutations from the **previous inference turn** — Write,
Edit, and Delete tool calls. One `/undo` per turn; call again to go back
another turn. Bash commands (e.g. `cargo build`) are **not** undoable.

```text
# Koda wrote bad code in the last turn
/undo    ← all file changes from that turn are reverted
/undo    ← undo the turn before that
```

## `/diff`

Shows a summary of uncommitted `git diff` in the project root. Then offers:

- **Review** — sends the diff to the model for code review comments
- **Commit** — asks the model to write a conventional commit message and
  runs `git commit -m "…"`

## `/sessions [<sub-command>]`

```text
/sessions              ← open the session picker (shows last 100 sessions)
/sessions resume abc   ← resume the session whose ID starts with "abc"
/sessions delete abc   ← permanently delete that session
```

Session IDs are UUIDs; you only need 6–8 characters to be unambiguous.
On resume, Koda shows an away-summary: idle time, message count, token
usage, and a banner if the previous turn was interrupted mid-inference.

## `/memory [save]`

```text
/memory        ← show the paths to project and global memory files
/memory save   ← ask the model to summarise the session and append to MEMORY.md
```

See [Memory](./memory.md) for the full memory system.

## `/skills [<query>]`

```text
/skills              ← list all built-in and custom skills
/skills security     ← filter by name or description
```

## `/agent <name>`

Switches to a named sub-agent for the current session. The agent's
system prompt, model, and allowed tools replace the current defaults.

```text
/agent testgen     ← use the "testgen" agent definition
```

## `/agents`

Lists currently-running **background tasks** — both background
sub-agents (the ones the model launched with `background: true` via
`InvokeAgent`) and background shell processes (spawned with
`Bash { background: true }`). Foreground sub-agents (the synchronous
`/agent <name>` switch above) don't appear here because they block
the conversation and are visible inline.

```text
  🐾 Background tasks

  ID            NAME              AGE     STATUS
  agent:1       explore           2m      ▶ Running (iter 8/20)
  agent:2       verify            45s     ◐ Pending
  process:5821  cargo test --w…   1m      ▶ Running
  process:5849  npm run dev       12s     ✓ Exited (0)
```

- `ID` — stable per-task id, **prefixed** by kind. `agent:N` for
  background sub-agents (assigned at spawn); `process:N` for
  background shell processes (the OS pid). Pass either form
  verbatim to `/cancel`.
- `NAME` — the sub-agent definition (for `agent:` rows) or a
  truncated head of the shell command (for `process:` rows).
- `AGE` — wall-clock time since the task was registered, rounded
  **down** (`1m` means "between 60 and 119 seconds").
- `STATUS` — latest value from the task's status channel:
  - Sub-agents: `Pending`, `Running { iter }`, `Cancelled`,
    `Completed`, or `Errored`.
  - Processes: `Running`, `Killed` (we sent SIGTERM but the OS
    hasn't reaped yet), or `Exited (code)` (`✓` for code 0, `✗`
    for any other code or signal exit).

The `iter` count comes from inside the inference loop; until that
wiring lands (Layer 4 of #996), in-progress tasks just show
`▶ Running` without the `(iter N/20)` suffix.

The LLM-facing equivalent is the `ListBackgroundTasks` tool — the
model sees the same data when it asks about its own background work.

## `/cancel <id>`

Requests cancellation of a background task by its `/agents` id.
Accepts three forms:

- `agent:N` — fires the per-task `CancellationToken`, which the
  inference loop checks between iterations. A cancelled sub-agent
  may run for one more iteration before noticing. The result still
  injects on the next user turn (with status `Cancelled` instead
  of `Completed`), so you don't lose any partial work the agent
  already did.
- `process:N` — sends SIGTERM to the shell process. The reaper
  transitions the entry to `Killed` immediately and to
  `Exited` once the OS confirms the process is gone.
- `N` (bare numeric) — back-compat with the original single-
  registry `/cancel` UX; treated as `agent:N`.

```text
/cancel agent:1       ← cancel background sub-agent #1
/cancel process:5821  ← SIGTERM background shell process pid 5821
/cancel 1             ← back-compat: same as `/cancel agent:1`
```

Idempotent: re-running `/cancel agent:1` on an already-cancelled
task is a no-op; `/cancel process:N` on an already-dead pid is also
a no-op (the kernel just rejects the SIGTERM). Unknown ids report a
helpful error rather than silently no-oping.

The LLM-facing equivalent is the `CancelTask` tool, which uses the
same parser and accepts the same prefixed forms.

## `/expand [<n>]`

Shows the full, untruncated output of a recent tool call. Useful when Koda
collapsed a long `cargo build` or `grep` result during streaming.

```text
/expand      ← show full output of the most recent tool call
/expand 3    ← show full output of the 3rd most recent tool call
```

## `/copy [<n>]`

Copies the Nth-most-recent **assistant response** to the system clipboard.
Defaults to the most recent response (`n=1`).

```text
/copy      ← copy the last response
/copy 2    ← copy the second-to-last response
/copy 5    ← copy the fifth-to-last response
```

Reads from the full session DB, so compacted (summarised) responses are
included in the count. A one-line preview is shown in the confirmation.

## `/export [<file.md>]`

Exports the full session transcript as a Markdown document.

```text
/export                    ← auto-named file in the current directory
/export notes/session.md   ← write to a specific relative path
```

Paths must be relative to the current directory. Absolute paths and `..`
traversal are rejected.

When no path is given, the filename is derived from the first user message
and the current UTC time:

```text
koda-20260410-143022-refactor-the-auth-module.md
```

The transcript includes all user messages, assistant responses, and a
summary of every tool call. System prompts are excluded.

## `/verbose [on|off]`

Toggles verbose tool output. By default Koda collapses long outputs
during streaming. Verbose mode shows every line in real time.

```text
/verbose      ← toggle
/verbose on   ← enable explicitly
/verbose off  ← disable explicitly
```

## `/exit`

Quit Koda. Equivalent to `Ctrl+D`.

## `/mcp <sub-command>`

Manage external MCP (Model Context Protocol) servers. See [MCP servers](./mcp.md)
for the full reference.

```text
/mcp list                                  ← list configured servers + status
/mcp add <name> <command> [args...]        ← add a stdio server
/mcp add-http <name> <url> [--token <tok>] ← add an HTTP server
/mcp reconnect <name>                      ← reconnect without restart
/mcp remove <name>                         ← permanently delete a server
```
