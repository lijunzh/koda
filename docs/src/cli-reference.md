# CLI reference

`koda` opens the TUI. `koda "prompt"` opens it and sends the prompt, as
`codex "prompt"` does; `koda exec "prompt"` runs one prompt without it.

## Flags

| Flag | Description |
|------|-------------|
| `-a`, `--agent <NAME>` | Agent to use: a built-in, or a JSON file in `.koda/agents/` or `~/.config/koda/agents/` ([Custom agents](./agents.md)); without it the config file's `agent`, else `default` |
| `-s`, `--resume <ID>` | Resume a previous session by its full ID, or by the start of one of this project's sessions, as `/resume` takes it. |
| `--model <NAME>` | Model ID (e.g. `claude-sonnet-5-5`) or alias (e.g. `claude-sonnet`); an alias also picks its provider. |
| `--provider <NAME>` | LLM provider, by its id or its name as `/provider` lists it (`anthropic`, `lm-studio` or `LM Studio`, `ollama`, …); one Koda doesn't offer is refused, with the list |
| `--base-url <URL>` | Override the provider's API base URL |
| `--max-context-tokens <N>` | Pin the context window, in tokens, for a server that doesn't report its own (a `rec` or other OpenAI-compatible engine). Beats the agent config's `max_context_tokens`, the config file's `model_context_window`, the server's answer and the model table, and sticks across `/model` switches. Must be a positive whole number; `0` or text is an error. |
| `--reasoning-effort <L>`, `--effort <L>` | Reasoning effort. Claude: `low`, `medium`, `high`, `xhigh` or `max`. Without it, each Claude model gets the level Claude Code uses: `medium` for the 5.5 models, `high` for Fable and the 5.0 models. A level the model doesn't take is refused. OpenAI-compatible servers get it as `reasoning_effort`. |
| `-c`, `--config <KEY=VALUE>` | Override a [config file](./configuration.md#the-config-file) key for this run, as Codex's `-c` does: a dotted key and a value read as TOML (else as text), which replaces what is at that key. Repeatable, never saved; an unknown key stops the run. Most flags here are `-c` values themselves (`--reasoning-effort` is `model_reasoning_effort`; see [the list](./configuration.md#the-config-file)), and win over one for the same key. |
| `--mode <MODE>` | [Trust mode](./approval.md): without it the config file's `mode`, else `auto`; `safe`; or `plan`, read-only: reads, searches and read-only commands run, everything else is refused (in the TUI, `/plan` and `Shift+Tab` switch to it too; see [Headless mode](./headless.md#read-only-runs) for `koda exec`). Auto auto-approves mutating actions; the kernel sandbox contains them — destructive ops (`rm -rf`, `git reset --hard`, `git push --force`) and outside-project writes still confirm. Auto requires the kernel sandbox; on unsandboxed platforms koda refuses to start (use `--mode safe` or install the platform sandbox backend). Safe confirms every mutation. Kernel sandbox with credential protection is always active when available. |
| `--oss` | Run on a local open-weight model, as Codex's `--oss`: Koda starts llama.cpp's `llama-server` with Qwen3.8-Flash-Next on a Mac with 128 GB, else (Linux included) the 1 GB LFM3-1.7B quick try, and stops it when Koda exits; on macOS it first checks the GPU isn't busy. `oss_model` picks another model. Can't be combined with `--provider` or `--base-url`. See [Local models](./local-models.md#one-command-koda---oss) |
| `--worktree` | Run the session in a new managed Git worktree, as Codex's `--worktree`: a detached checkout of the project's `HEAD` in `~/.config/koda/worktrees/<id>/<project>`, outside the repository. For new sessions (`koda`, `koda exec`), not `resume` or `fork`. Koda keeps it when it exits and prints how to remove it (`git worktree remove <path>`). See [Sessions](./sessions.md#a-session-in-its-own-worktree) |
| `--local-provider <NAME>` | With `--oss`: `llama-cpp` (the default, started and stopped by Koda), `lm-studio` or `ollama` (connected to) |
| `--mcp-config <FILE\|JSON>` | Add MCP servers for this run only, from a JSON file or inline JSON in the `{"mcpServers": {...}}` shape of Claude Code's `.mcp.json`. Repeatable. Nothing is saved; a bad value stops the start. See [MCP servers](mcp.md). |
| `--project-root <DIR>` | Project root (defaults to cwd) |
| `-i`, `--image <FILE>` | Attach an image to the first prompt, as Codex's `-i`: repeatable, or several separated by commas. See [Images](./attachments.md#images) |

**Which command takes which flag,** as in Codex: `koda` and `koda exec`
take them all. `koda serve` takes the config flags but not `--resume`,
`--model`, `--provider` or `--base-url`, which its client sends in
`session/start`. `koda mcp` takes only `--project-root` and `-c`. A flag
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
| `koda exec [PROMPT]` | Run one prompt and exit, as `codex exec` does. `--json` prints the [client protocol](headless.md#the-client-protocol) on stdout, one JSON-RPC message per line; `--json-deltas` adds streaming text and command output; `-o FILE` writes the last reply to a file. `-` or no prompt reads stdin. Takes every flag above, after `exec` or before it |
| `koda resume [SESSION] [PROMPT]` | Resume a saved session in the TUI, as `codex resume` does: by its ID or the start of one; `--last` for the one with the latest activity (then a single argument is the prompt); with neither, the session list opens. Takes `koda`'s flags. See [Sessions](./sessions.md) |
| `koda fork [SESSION] [PROMPT]` | Fork a saved session and open the copy in the TUI, as `codex fork` does: the same choice of session as `koda resume` (an ID or its start, `--last`, or the list). The original stays as it was. Takes `koda`'s flags; `--model` and `--provider` override the copy's. See [Sessions](./sessions.md#forking-a-session) |
| `koda exec resume [SESSION] [PROMPT]` | Run a prompt in a saved session, as `codex exec resume` does: by ID, or `--last`. Takes `koda exec`'s flags |
| `koda serve --stdio` | Serve the [client protocol](serve.md) to one program on stdin and stdout, one JSON-RPC message per line, as `codex app-server` does: it starts sessions, runs turns and answers approvals. Runs until stdin closes or a signal stops it. Takes `-c` and the config flags, not `--resume`, `--model`, `--provider` or `--base-url`: the client names each session's model in `session/start` |
| `koda mcp list\|get\|add\|remove` | Manage the MCP servers every session starts, as `codex mcp` does: `add <name> -- <command> [args]` (with `--env KEY=VALUE`) or `add <name> --url <url>` (with `--bearer-token-env-var VAR`); `list` and `get` take `--json`. Open TUI sessions take a change on `/mcp reconnect`. Takes only `--project-root` and `-c`. See [MCP servers](./mcp.md) |
| `koda --version` | Print the version |
