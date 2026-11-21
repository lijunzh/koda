# CLI reference

`koda` starts an interactive session. `koda "prompt"` starts one with a first
prompt. `koda exec "prompt"` runs one prompt without an interactive session,
prints the reply and exits.

## Flags

| Flag | Description |
|------|-------------|
| `-m`, `--model <NAME>` | Model to use: an ID (such as `claude-sonnet-5-5`) or an alias (such as `claude-sonnet`), which also picks its provider |
| `--provider <NAME>` | Provider, by ID or by the name `/provider` lists (`anthropic`, `lm-studio` or `LM Studio`, `ollama`, …). One Koda doesn't offer is refused, with the list |
| `--base-url <URL>` | The provider's endpoint, for a local server or a gateway |
| `--oss` | Run on a local model that Koda starts with llama.cpp's `llama-server`: Qwen3.8-Flash-Next on a Mac with 128 GB of memory, else (Linux included) LFM3-1.7B, a 1 GB download for a quick try. Koda stops the server when it exits; on macOS it first checks the GPU isn't busy. `oss_model` in the config file picks another model. Can't be combined with `--provider` or `--base-url`. See [Local models](./local-models.md#one-command-koda---oss) |
| `--local-provider <NAME>` | With `--oss`: `llama-cpp` (the default; Koda starts and stops it), or `lm-studio` or `ollama`, which must already be running |
| `--reasoning-effort <L>`, `--effort <L>` | How much the model reasons. Claude models take `low`, `medium`, `high`, `xhigh` or `max`, and each has its own default: `medium` for the 5.5 models, `high` for Fable and the 5.0 models. A level the model doesn't take is refused. An OpenAI-compatible server gets the value as `reasoning_effort` |
| `--mode <MODE>` | What Koda may do without asking ([Trust modes](./approval.md)). `auto`, the default: changes run without asking, inside the sandbox; destructive commands (`rm -rf`, `git reset --hard`, `git push --force`) and writes outside the project still ask. Auto needs the sandbox; where there is none, Koda refuses to start in it (use `--mode safe`). `safe`: every change asks first. `plan`: read only; reads, searches and read-only commands run, everything else is refused, and in the TUI Koda proposes a plan and asks before carrying it out (`/plan` and `Shift+Tab` switch to it too; see [Headless mode](./headless.md#read-only-runs) for `koda exec`). The config file's `mode` sets the default |
| `-i`, `--image <FILE>` | Attach images to the first prompt: repeatable, or several separated by commas. See [Images](./attachments.md#images) |
| `-C`, `--cd <DIR>`, `--project-root <DIR>` | Directory to work in (default: the current directory) |
| `--worktree` | Work in a new Git worktree: a detached checkout of the project's `HEAD` in `~/.config/koda/worktrees/<id>/<project>`, outside the repository. For new sessions (`koda`, `koda exec`) and copies of a session named by ID (`koda fork <id> --worktree`, `koda exec fork <id> --worktree`, which check out the repository that session worked in), not `resume`. It stays after Koda exits, and Koda prints how to remove it (`git worktree remove <path>`). See [Sessions](./sessions.md#a-session-in-its-own-worktree) |
| `-c`, `--config <KEY=VALUE>` | Set a [config file](./configuration.md#the-config-file) value for this run: a dotted key, and a value read as TOML (else as plain text) that replaces what is at that key. Repeatable and never saved; an unknown key stops the run. Most flags here set a config value themselves (`--reasoning-effort` is `model_reasoning_effort`; see [the list](./configuration.md#the-config-file)) and win over `-c` for the same key |
| `--mcp-config <FILE\|JSON>` | Add MCP servers for this run only, from a JSON file or inline JSON in the `{"mcpServers": {...}}` format of `.mcp.json` files. Repeatable. Nothing is saved; a bad value stops the start. See [MCP servers](mcp.md) |
| `-a`, `--agent <NAME>` | Agent to run: a built-in one, or a JSON file in `.koda/agents/` or `~/.config/koda/agents/` ([Custom agents](./agents.md)). The default is the config file's `agent`, else `default` |
| `--max-context-tokens <N>` | The context window, in tokens, for a server that doesn't report its own. Overrides the agent file's `max_context_tokens`, the config file's `model_context_window`, the server's report and Koda's model table, and holds across `/model` switches. A positive whole number; `0` or text is an error |
| `-s`, `--resume <ID>` | Resume a saved session by its full ID, or by the start of one of this project's sessions, as `koda resume <ID>` does |
| `--no-daemon` | Run in this process instead of on the [background server](serve.md#the-tui-on-the-shared-server), which an interactive session otherwise uses so it goes on after its window closes. `--oss` and `-c` values do the same |
| `--remote unix://PATH` | Connect to a background server at that socket instead of Koda's own. Koda doesn't start or replace it |

**Which command takes which flag:** `koda` and `koda exec`
take them all. `koda serve` takes the config flags but not `--resume`,
`--model`, `--provider` or `--base-url`, which its client sends in
`session/start`. `koda mcp` takes only `-C`/`--cd` and `-c`. A flag
given before `exec` (`koda --model x exec "…"`) carries over to it; one
given before a subcommand that can't use it is an error, not ignored.

**Deprecated, going in 0.8:** `--max-tokens`, `--temperature` and
`--thinking-budget` still work, with a warning; use
`-c model_max_output_tokens=N`, `-c model_temperature=X` and
`-c model_thinking_budget=N`.

Coming from 0.6, where `koda -p` and `--output-format` ran a prompt, see
[Upgrading to 0.7](./upgrading-0.7.md).

## Subcommands

| Command | Description |
|---------|-------------|
| `koda exec [PROMPT]` | Run one prompt and exit. The reply goes to stdout, progress to stderr. `--json` prints the [client protocol](headless.md#the-client-protocol) on stdout instead, one JSON-RPC message per line; `--json-deltas` adds streaming text and command output; `-o FILE` writes the last reply to a file. `-` or no prompt reads stdin. Takes every flag above, after `exec` or before it |
| `koda resume [SESSION] [PROMPT]` | Resume a saved session: by its ID or the start of one; `--last` for the one with the latest activity (then a single argument is the prompt); with neither, the session list opens. Takes `koda`'s flags. See [Sessions](./sessions.md) |
| `koda fork [SESSION] [PROMPT]` | Copy a saved session and continue in the copy, chosen as for `koda resume` (an ID or its start, `--last`, or the list). The original is left unchanged. Takes `koda`'s flags; `--model` and `--provider` change the copy's, and with an ID, `--worktree` opens the copy in a new worktree. See [Sessions](./sessions.md#forking-a-session) |
| `koda exec resume [SESSION] [PROMPT]` | Run a prompt in a saved session, by ID or `--last`. Takes `koda exec`'s flags |
| `koda exec fork <SESSION> [PROMPT]` | Run a prompt in a copy of a saved session, by ID or its start. Without a prompt, only copy it and print the copy's ID. Takes `koda exec`'s flags; with `--worktree`, the copy runs in a new worktree of the session's repository. See [Headless mode](./headless.md#forking-a-session-in-headless-mode) |
| `koda mcp list\|get\|add\|remove\|login\|logout` | Manage the MCP servers every session starts: `add <name> -- <command> [args]` (with `--env KEY=VALUE`) or `add <name> --url <url>` (with `--bearer-token-env-var VAR`); `list` and `get` take `--json`; `login <name>` signs in to an HTTP server with OAuth and `logout <name>` forgets the sign-in ([Signing in with OAuth](./mcp.md#signing-in-with-oauth)). Open sessions pick up a change after `/mcp reconnect`. Takes only `-C`/`--cd` and `-c`. See [MCP servers](./mcp.md) |
| `koda serve [--listen unix://PATH]` | The [background server](serve.md#the-shared-server) interactive sessions use: the client protocol to several clients at once on a Unix socket, `~/.config/koda/serve/koda.sock` unless `--listen` names another. `koda` starts it when needed. Each client hears the sessions it starts; a session outlives its client and closes after a minute idle with none. Runs until a signal stops it. Takes `-c` and the config flags, not `--resume`, `--model`, `--provider` or `--base-url`: the client names each session's model in `session/start` |
| `koda serve --stdio` | The [client protocol](serve.md) to one program on stdin and stdout, one JSON-RPC message per line: it starts sessions, runs turns and answers approvals. Runs until stdin closes or a signal stops it |
| `koda serve --stop` | Stop the background server at the socket (`--listen` names another). Its open sessions close and stay saved. Exits 1 when none is listening |
| `koda --version` | Print the version |
