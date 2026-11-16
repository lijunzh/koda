# Upgrading to 0.7

Koda 0.7 puts every client (the TUI, `koda exec`, `koda serve`) on one client
protocol, as Codex does, and follows Codex for its command line and settings.
This page lists what you may need to change coming from 0.6. The
[changelog](https://github.com/lijunzh/koda/blob/main/CHANGELOG.md) has
everything, release by release.

## Running Koda

| In 0.6 | In 0.7 |
|---|---|
| `koda -p "prompt"` | `koda exec "prompt"` |
| `koda "prompt"` (one-shot) | `koda exec "prompt"`; `koda "prompt"` now opens the TUI and sends it |
| `git diff \| koda` | `git diff \| koda exec` |
| `--output-format json` | `koda exec --json`; the summary is its last line, `exec/summary`, in camelCase (`sessionId`, `turnStatus`, `durationMs`, `usage.inputTokens`, …) |
| `--output-format stream-json` | `koda exec --json`, which streams the [client protocol](./headless.md#the-client-protocol): items that start and complete, every tool call included |
| `koda resume` (sent "resume" as a prompt) | `koda resume` opens the session list; `koda resume --last` the latest in this project; `-s`/`--resume` still work |
| `koda server` (ACP) | Removed; see [ACP](#acp-is-gone) |

Each removed form stops with exit code 2 and says what to run instead. `koda`
with stdin piped stops with "stdin is not a terminal": the TUI needs one.

**Each subcommand takes only its own flags,** as Codex's do: `koda exec
--json` works, `koda --json` doesn't.

**Deprecated, going in 0.8:** `--max-tokens`, `--temperature` and
`--thinking-budget` still work, with a warning. Use `-c
model_max_output_tokens=N`, `-c model_temperature=X` and `-c
model_thinking_budget=N`, or set them in `config.toml`.

## Settings live in config.toml, not the environment

Koda no longer reads settings from environment variables, as Codex doesn't:
they live in `~/.config/koda/config.toml`, with `-c key=value` for one run (see
[Configuration](./configuration.md)). API keys such as `ANTHROPIC_API_KEY` are
still read. A retired variable that is still set gets a warning at startup
naming its replacement:

| Variable | Use instead |
|---|---|
| `KODA_MODEL` | `--model`, or `model` |
| `KODA_PROVIDER` | `--provider`, or `model_provider` |
| `KODA_BASE_URL`, `KODA_LOCAL_URL` | `--base-url`, or `[model_providers.<id>] base_url` |
| `KODA_MAX_CONTEXT_TOKENS` | `--max-context-tokens`, or `model_context_window` |
| `KODA_REASONING_EFFORT` | `--effort`, or `model_reasoning_effort` |
| `KODA_MODE` | `--mode`, or `mode` |
| `KODA_POST_TURN_COMPACT_PERCENT` | `model_post_turn_compact_threshold_percent` |
| `KODA_REQUEST_MAX_RETRIES`, `KODA_STREAM_MAX_RETRIES` | `[model_providers.<id>] request_max_retries`, `stream_max_retries` |
| `KODA_READ_TIMEOUT_SECS` | `[model_providers.<id>] stream_idle_timeout_ms` |
| `KODA_SYNTAX_HIGHLIGHT` | `[tui] syntax_highlight` |
| `KODA_ALLOW_LOCAL_BINDING` | `[sandbox] allow_local_binding` |
| `KODA_CONNECT_TIMEOUT_SECS` | Nothing: the connect timeout is a fixed 30 seconds |

**Flags are for one run.** A flag is never saved as the next start's choice;
`/model` and `/provider` save a choice into `config.toml`.

## In the TUI

| In 0.6 | In 0.7 |
|---|---|
| `/sessions` | `/resume` |
| `/agent` | `/agents` |
| `/debug-bundle` | `/feedback` |
| `/cancel` | `Esc` stops the turn and its sub-agents; `/stop` stops background processes |
| `/purge` | Delete a session from `/resume` |
| `/mcp add`, `/mcp remove` | `koda mcp add`, `koda mcp remove`; the slash forms print the command to run |

- **`Esc` and `Ctrl+C` stop the turn, not background processes.** A dev server
  the model started keeps running until `/stop`, until the model stops it, or
  until Koda exits.
- **Plan mode is in the TUI** (0.7.1): `/plan` or `Shift+Tab`, which now cycles
  Safe, Auto and Plan. A session left in Plan reopens in Plan; before, it moved
  to Safe.
- **New:** `/new` (`/clear`), `/rename`, `/status`, `/init`, `/fork`, and
  `koda fork` (0.7.1).
- **What you typed** is kept in `~/.config/koda/history.jsonl`, not the
  database.
- **Compacting after a turn is off by default,** as in Codex; context still
  compacts when it fills. Set `model_post_turn_compact_threshold_percent` to
  compact after a turn that ends past that share of the window.

## Projects and agents

- **Project agents are read from `.koda/agents/`,** as the docs always said;
  0.6 read `agents/` at the project root.
- **MCP tokens** saved by 0.6 (`/mcp add-http --token`) aren't read. Name the
  variable that holds the token with `--bearer-token-env-var` in `koda mcp
  add` (see [MCP servers](./mcp.md)).

## Models and platforms

- **Older Claude models** (Claude 2 and 3, Instant, the 4.x line) still run,
  with a warning on the first turn that requests may fail. Their `-1m` model
  names are refused with the name to use, and `--thinking-budget` is for Gemini
  only.
- **No Intel Mac binary** (0.7.1): install with `cargo install koda-cli
  --locked`; see [Installation](./installation.md).

## ACP is gone

Koda no longer has an ACP server: `koda server` and the `koda connect`
placeholder are removed, so editors that speak the [Agent Client
Protocol](https://agentclientprotocol.com) can't connect to Koda. If you
registered Koda as an agent in your editor, remove that entry; `koda server`
now exits with an error that points here.

It didn't work: the server stopped reading input while a turn ran, so the
first approval in Safe mode deadlocked; it ignored the workspace and session
a client asked for, and treated "always allow" as a rejection. Koda has one
protocol instead.

| You want to | Use |
|---|---|
| Work with Koda from an editor | Run `koda` in the editor's terminal |
| Call Koda from a script | `koda exec "prompt"` ([headless mode](./headless.md)) |
| Read a run's events from a program | `koda exec --json "prompt"` ([client protocol](./headless.md#the-client-protocol)) |
| Drive Koda from a program: sessions, turns, approvals | `koda serve --stdio` ([Koda as a server](./serve.md)) |

## For code that uses Koda's crates

- **Embed Koda through `koda-client`**, the client API the TUI and `koda exec`
  use. `koda-cli` no longer depends on `koda-core`, and `koda-core`'s API makes
  no promise of stability.
- **New crates** split out of `koda-core`: `koda-protocol`, `koda-client`,
  `koda-server`, `koda-config`, `koda-state`, `koda-providers`,
  `koda-command`, `koda-mcp`, `koda-skills` and `koda-memory`. `koda-core`
  re-exports the old paths where it could.
- **The `test-support` Cargo feature is gone** (0.7.1); test helpers are in the
  unpublished `koda-test-utils`.
