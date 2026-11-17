# Slash commands

Type in the TUI input. Tab-completion is available for all commands.

## `/help`

Shows the quick-reference keybinding card inside the TUI.
This docs site is the full reference; `/help` is the in-session reminder.

## `/model [<alias-or-id>]`

Without an argument: opens a picker of the session's provider's models,
its aliases first (for Anthropic, the newest model of each line). As in
Codex, a session keeps its provider: another provider's model is
`/provider`, which opens a new session on it. In the picker, as in Codex's:

| Key | Switches | Saved as the default in config.toml |
|---|---|---|
| Enter | this session | yes |
| `s` | this session | no |

With an argument: switches immediately, for the session's next turns, and
saves it as the default, as Enter does.

```text
/model claude-opus         ← switch by alias (an Anthropic session)
/model gpt-4o              ← literal model ID (an OpenAI session)
/model llama3.2            ← any model name your provider understands
```

The session records its model, so resuming it brings it back, whichever key
picked it. A default is remembered for new sessions until changed again. See
[Providers & model aliases](./providers.md).

## `/provider [<name or URL>]`

Without an argument: opens a two-step picker — choose provider, then
browse and pick one of its available models. Another provider, or another
address, starts a new session on it, as switching provider does in Codex;
the session you left stays in `/resume`. A model of the session's own
provider just switches the model.

With an argument: jumps straight to that provider's model list.

```text
/provider                  ← open the picker
/provider anthropic        ← go straight to Anthropic models
/provider ollama           ← browse locally running Ollama models
/provider http://127.0.0.1:8080/v1   ← any OpenAI-compatible server
```

A URL sets the endpoint directly. Koda detects the provider type from it
(`http://localhost:8080/v1` is llama.cpp, other local ports are LM Studio),
picks up the loaded model and remembers the choice in
`~/.config/koda/config.toml`, the endpoint as the provider's
`[model_providers.<id>] base_url`. Flags such as `--base-url` or `--model`
apply to that run only and are never saved, as Codex's aren't; headless runs
(`koda exec`) never save anything.

## `/key`

Opens the API key manager. Select a provider, then type or paste your key.
Keys are stored in the local SQLite keystore (file mode 0600) and injected
as environment variables at every startup.

The input is rendered with each character masked
as `•` while you type, so a peer glancing at your terminal won't see the
key itself — useful for screen-sharing or pair-coding sessions. The
composer still tracks the real characters under the hood; backspace and
paste behave normally.

Shell env vars always win over stored keys — so `export ANTHROPIC_API_KEY=…`
in your shell or `.envrc` is always a clean override.

## `/compact`

Summarises old conversation history to free context tokens. Koda
auto-compacts when the context window hits **85%** full, but you can
trigger it manually at any time. It runs like a turn: Esc stops it, leaving
the history as it was.

- The newest **30% of the history by size** stays as it is (at least 4 messages)
- The rest is summarised by the model, and your own messages from that part
  are kept word for word beside the summary
- The summary replaces the old messages in the DB
- The compressed session continues normally
- The replaced messages stay in the database, marked as compacted

## `/diff`

Shows a summary of uncommitted changes in the project root: `git diff`, and
new files git doesn't track yet, as Codex's `/diff` includes them. Then offers:

- **Review** — sends the diff, new files included, to the model for code review comments
- **Commit** — asks the model to write a conventional commit message and
  runs `git commit -m "…"`

## `/resume [<id> | delete <id>]`

```text
/resume                ← open the session picker (shows last 100 sessions)
/resume abc            ← resume the session whose ID starts with "abc"
/resume delete abc     ← permanently delete that session
```

Named `/sessions` before; that name still works and says the new one. A
resumed session reopens on its own provider and model, as in Codex.

Session IDs are UUIDs; you only need 6–8 characters to be unambiguous.
On resume, Koda draws the session's earlier turns as they were drawn live,
as Codex does, then a `session resumed` line, and a banner if the last turn
was interrupted.

## `/new` (also `/clear`)

Starts a new session on the same model and mode, as Codex's `/new` and
Claude Code's `/clear` do. The session you left stays stored, for
`/resume`.

## `/fork`

Copies the session into a new one and switches to it, as Codex's `/fork`
does: same history, model and mode, named "Fork of" this session's name.
The original stays as it was, for `/resume`. See
[Forking a session](./sessions.md#forking-a-session).

## `/rename <name>`

Names the session, as Codex's `/rename` does; `/resume` lists it by that
name. Without one, the first 50 characters of your first prompt are its
title, and a name you give is never replaced by it.

## `/status`

Shows what the session runs with, as Codex's `/status` does: its ID and
name, the model, provider and endpoint, reasoning effort, mode, sandbox,
how much of the context window is left, the project, the instruction files
loaded, and Koda's version.

## `/init`

Asks the model to write an `AGENTS.md` for the project, a short guide for
coding agents, as Codex's `/init` and Claude Code's do. If the project has an
`AGENTS.md` or `CLAUDE.md` already, `/init` leaves it alone and says so.

## `/plan [<prompt>]`

Enters Plan mode, as Codex's `/plan` does; with a prompt, sends it too.
The model explores, asks what it can't find out, and proposes a plan;
Koda then asks whether to implement it in Safe or Auto, or to keep
planning. Nothing changes in Plan. `Shift+Tab` leaves it. See
[Planning before changing anything](./approval.md#planning-before-changing-anything).

## `/memory [add|global <text>]`

```text
/memory                 ← show the instruction files the session reads, and where Koda's notes are
/memory add <text>      ← add to Koda's notes for this project
/memory global <text>   ← add to Koda's global notes
```

See [Memory](./memory.md) for the full memory system.

## `/skills [<query>]`

```text
/skills              ← list all built-in and custom skills
/skills security     ← filter by name or description
```

## `/agents`

Lists the available agents: built-in, yours (`~/.config/koda/agents/`) and
the project's (`.koda/agents/`). To run a session as a named agent, start
Koda with `--agent <name>`. `/agent`, its old name, still works for a
release and says the new one.

```text
/agents
koda --agent testgen
```

## `/stop`

Stops every background process the model started (`Bash` with
`background: true`), such as a dev server. `/clean` is the same command, as in
Codex. It works during a turn too. `Esc` and `Ctrl+C` stop the turn and its
sub-agents but leave background processes running; they also stop when Koda
exits. To stop just one, ask: "stop the dev server". Koda stops a background
process it started itself, without asking in Auto mode.

The **activity overlay** just above the status bar shows, in real time, the
sub-agents of the current turn and every background process: name, the last
tool call each sub-agent fired, the command each process runs, and its age.
Its last row says what stops them.

```text
  🤖 explore   (12s)  · Read src/lib.rs
  🐚 process:5821 (1m) · npm run dev
  Esc to interrupt · /stop to stop background processes
```

## `/expand [<n>]`

Shows the full output of one recent tool call again at the bottom. To see
every tool call's output in place, press `Ctrl+O` instead.

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

Counts the session's own replies, not sub-agents', from the ones the TUI was
sent: this run's, and a resumed session's earlier turns, compacted ones
included. A one-line preview is shown in the confirmation.

## `/feedback`

Saves a report about this session as a `.zip` you can attach to an issue, as
Codex's `/feedback` does. It asks three things:

1. **How was this?** `bug`, `bad result`, `good result` or `other`.
2. **Include logs and session history?** They may hold your prompts, commands
   and tool output. Say no and the ZIP holds only the report.
3. **A note** (optional): what happened, in your words.

It saves to `~/.config/koda/feedback/koda-feedback-<time>-<id>.zip`, readable
by you only, and prints the path. For anything but a good result it also gives
the link to open an issue. Nothing is sent anywhere.

| File | What's in it |
|---|---|
| `report.json` | The category and note, the session, Koda's version, OS and time |
| `attachments/…/koda-logs.log` | The session's lines from the log database, its sub-agents' too, and this process's startup lines (with consent) |
| `attachments/…/session-<id>.json` | The session's stored rows, and each sub-agent session's: its session row, every message (compacted ones too), its turns, their items and settings, and its events (with consent) |
| `attachments/…/diagnostics.txt` | Versions, OS, sandbox, whether the log lost lines, and the environment (with consent) |
| `manifest.json` | What each attachment is |

The environment goes through an allowlist (`koda-server/src/feedback/env_filter.rs`):

- **Kept as is:** `KODA_*`, `RUST_*`, `LC_*`, `LANG`, `TERM`, `SHELL`, `PATH`.
- **Kept by length only:** known keys such as `ANTHROPIC_API_KEY` and
  `GITHUB_TOKEN`, and the proxy variables, as `<redacted, N bytes>`.
- **Left out:** everything else, including `HOME`, `USER` and `PWD`.

Any client can save one with the `feedback/save` request (Codex's), which takes
the category, note, session, whether to include logs, and an absolute path
that doesn't exist yet.

## `/verbose [on|off]`

Switches tool output between folded and full, the same as `Ctrl+O`. Koda
folds tool output by default: a file read shows as one line (`└ 123
lines`), and other output as its first five lines and a `… +N lines`
marker, plus the last five when the call failed. Edits and writes show
their diff. The full view shows every line, for past output as well as new,
in a resumed session too.

```text
/verbose      ← switch
/verbose on   ← full view
/verbose off  ← folded view
```

## `/vim`

Toggles vim-mode editing in the input composer. When enabled, the
composer behaves like a single-buffer vi: `Esc` enters Normal mode,
`i`/`a`/`o` re-enter Insert mode, and the usual `h j k l`, `w b e`,
`0 $`, `gg G`, `dd yy p`, `x`, `cc`, `ci<delim>`, `ca<delim>`, etc.
bindings are honored. Useful when you're already living in vim and
want the same muscle memory in koda's chat input.

The setting is per-session only; relaunching
koda starts back in plain insert mode.

See also: [Keybindings → Vim mode](./keybindings.md#vim-mode).

## `/exit`

Quit Koda. Equivalent to `Ctrl+D`. `/quit` works too.

## `/mcp`

The session's MCP (Model Context Protocol) servers. Servers are added and
removed from a terminal with `koda mcp`, as Codex's are with `codex mcp`. See
[MCP servers](./mcp.md) for the full reference.

```text
/mcp              ← this session's servers, their status and tools
/mcp verbose      ← the same, with each server's config and tool descriptions
/mcp reconnect    ← take servers added or removed with `koda mcp`; retry failed ones
```
