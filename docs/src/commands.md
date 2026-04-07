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

## `/expand [<n>]`

Shows the full, untruncated output of a recent tool call. Useful when Koda
collapsed a long `cargo build` or `grep` result during streaming.

```text
/expand      ← show full output of the most recent tool call
/expand 3    ← show full output of the 3rd most recent tool call
```

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
