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

As of v0.2.27 (#1185), the input is rendered with each character masked
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

Shows a summary of uncommitted `git diff` in the project root. Then offers:

- **Review** — sends the diff to the model for code review comments
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
On resume, Koda shows an away-summary: idle time, message count, token
usage, and a banner if the previous turn was interrupted mid-inference.

## `/memory [save]`

```text
/memory        ← show the instruction file and where Koda's notes are
/memory add <text>      ← add to Koda's notes for this project
/memory global <text>   ← add to Koda's global notes
```

See [Memory](./memory.md) for the full memory system.

## `/skills [<query>]`

```text
/skills              ← list all built-in and custom skills
/skills security     ← filter by name or description
```

## `/agent`

Lists the available agents, built-in and custom. To run a session as a
named agent, start Koda with `--agent <name>`.

```text
/agent
koda --agent testgen
```

## `/agents`

Removed in #1210. The **activity overlay** just above the status bar
shows, in real time, the sub-agent running in the current turn and
every background shell process: name, the last tool call each one
fired, and elapsed age.

```text
  🤖 explore   (12s)  · Read src/lib.rs
  🐚 process:5821 (1m) · cargo test --workspace
  + 2 more  ·  Esc cancel all
```

## `/cancel`

Removed on 2026-11-06. It only ran while Koda was idle, when no
sub-agent exists, so `/cancel agent:N` could never reach one. Press
`Esc` or `Ctrl+C` to stop everything running, including background
shell processes. To stop just one background process, ask: "stop the
dev server". Koda stops a background process it started itself, without
asking in Auto mode.

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

Reads from the full session DB, so compacted (summarised) responses are
included in the count. A one-line preview is shown in the confirmation.

## `/debug-bundle`

Writes a self-contained `.zip` artifact capturing everything a debugger
(human or LLM) needs to reason about the current session. Replaces the
`/export` command removed in v0.2.26 (RFC #1167).

```text
/debug-bundle
```

The bundle is written to:

```text
~/.config/koda/debug-bundles/koda-debug-{timestamp}-{slug}.zip
```

where `{slug}` is derived from the first user message. The success
message prints the full path and a hint to the raw log directory:

```text
Wrote debug bundle → /Users/you/.config/koda/debug-bundles/koda-debug-20260429-082303-fix-the-auth-bug.zip
  raw logs: /Users/you/.config/koda/logs/latest
```

### Bundle layout

| File | What's in it |
|---|---|
| `README.md` | Human-orientation: what each file is, when to share, the env-var redaction caveat |
| `conversation.md` | Full session rendered via the same `history_render` pipeline as the live TUI — byte-for-byte identical to what you saw on screen |
| `messages.json` | Raw per-message DB rows (role, content, tool calls, tool results) |
| `metadata.json` | Session ID, title, started-at, model, provider, current PID, capture timestamp |
| `env.txt` | Allowlist-filtered environment variables (see below) |
| `logs/koda-{PID}.log` | Full per-process tracing log captured by the active session |
| `logs/panic.log` | A copy of `~/.config/koda/logs/panic.log` if it exists, so it can hold panics from other sessions |

The `.zip` format was chosen for **random-access reads** — LLM debugging
workflows that poke at one file at a time pay O(file) instead of
O(bundle), and the artifact opens cleanly in Finder Quick Look,
Windows Explorer, and any unzip CLI.

### Environment-variable redaction

`env.txt` is filtered through an **allowlist by default** in
`koda-cli/src/debug_bundle/env_filter.rs`. The filter has three
categories:

- **Pass through verbatim**: `KODA_*`, `RUST_*`, `LC_*`, `LANG`,
  `TERM`, `SHELL`, `PATH`, proxy vars (`HTTP_PROXY` / `HTTPS_PROXY` /
  `NO_PROXY`).
- **Length-redact**: known credential vars (`OPENAI_API_KEY`,
  `ANTHROPIC_API_KEY`, `GITHUB_TOKEN`, `GEMINI_API_KEY`, etc.) appear
  as `<redacted, N bytes>` so their *presence* is visible to the
  debugger but the *value* never leaks.
- **Drop entirely**: PII (`HOME`, `USER`, `PWD`, `LOGNAME`) and any
  unknown variable. Defaulting unknown to *drop* protects against
  unanticipated future credential names.

There is **no runtime opt-in to disable this filter**. Embedding raw
`std::env::vars()` in a sharable artifact is a foot-gun even with
opt-in flags; the bundle README also reminds you to re-check `env.txt`
before sharing externally.

### When to use it

- Reporting a bug to the koda issue tracker — attach the `.zip` so
  maintainers see your exact session, panic context, and runtime
  environment.
- Asking another LLM (Claude, ChatGPT, Gemini) to help debug a koda
  session — the model can `unzip` and read `conversation.md` directly.
- Capturing a known-bad state for later analysis (the timestamped
  filename + slug make bundles trivially sortable).

### Self-correlating logs (v0.2.26)

When a panic occurs, the panic hook now emits a `tracing::error!`
breadcrumb in rustc's default format (`thread '<name>' panicked at
<location>: <message>`) **before** writing `panic.log`. The breadcrumb
lands in the per-process tracing log, so within a `/debug-bundle` the
`logs/panic.log` and `logs/koda-{PID}.log` are correlatable by
wall-clock timestamp — no more orphan panic files disconnected from
the events that led up to them.


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

Added in v0.2.27 (#1182). The setting is per-session only; relaunching
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
