# CLI reference

## Flags

| Flag | Env var | Description |
|------|---------|-------------|
| `-a`, `--agent <NAME>` | | Agent definition to use (JSON in `agents/`, default: `default`) |
| `-s`, `--resume <ID>` | | Resume a previous session by its full ID. `/sessions resume` in the TUI takes a prefix. |
| `--model <NAME>` | `KODA_MODEL` | Model ID (e.g. `claude-sonnet-5-5`) or alias (e.g. `claude-sonnet`); an alias also picks its provider. |
| `--provider <NAME>` | `KODA_PROVIDER` | LLM provider (`anthropic`, `gemini`, `openai`, `ollama`, …) |
| `--base-url <URL>` | `KODA_BASE_URL` | Override the provider's API base URL |
| `--max-tokens <N>` | | Maximum output tokens |
| `--max-context-tokens <N>` | `KODA_MAX_CONTEXT_TOKENS` | Pin the context window, in tokens, for a server that doesn't report its own (a `rec` or other OpenAI-compatible engine). Beats the agent config's `max_context_tokens`, the server's answer and the model table, and sticks across `/model` switches. Must be a positive whole number; `0` or text is an error. |
| `--temperature <F>` | | Sampling temperature (0.0–2.0) |
| `--thinking-budget <N>` | | Anthropic thinking budget in tokens, for Claude models that still take one (Haiku 4.5 and older). Current Claude models refuse it; use `--effort`. |
| `--effort <L>`, `--reasoning-effort <L>` | `KODA_REASONING_EFFORT` | Reasoning effort. Claude: `low`, `medium`, `high`, `xhigh` or `max`. Without it, each Claude model gets the level Claude Code uses: `medium` for the 5.5 models, `high` for Fable and the 5.0 models. A level the model doesn't take is refused. OpenAI-compatible servers get it as `reasoning_effort`. |
| `--mode <MODE>` | `KODA_MODE` | Trust mode: `auto` (default, #1241), `safe`, or `plan` (`koda exec` only: read-only, everything else refused; see [Headless mode](./headless.md#read-only-runs)). Auto auto-approves mutating actions; the kernel sandbox contains them — destructive ops (`rm -rf`, `git reset --hard`, `git push --force`) and outside-project writes still confirm. Auto requires the kernel sandbox; on unsandboxed platforms koda refuses to start (use `--mode safe` or install the platform sandbox backend). Safe confirms every mutation. Kernel sandbox with credential protection is always active when available. |
| `--mcp-config <FILE\|JSON>` | | Add MCP servers for this run only, from a JSON file or inline JSON in the `{"mcpServers": {...}}` shape of Claude Code's `.mcp.json`. Repeatable. Nothing is saved; a bad value stops the start. See [MCP servers](mcp.md). |
| `--project-root <DIR>` | | Project root (defaults to cwd) |

`koda -p`, `--output-format` and a prompt given to `koda` itself were removed in
0.7.0; `koda exec` runs a prompt. `koda` with stdin piped stops with the same
pointer, since the TUI needs a terminal.

## Subcommands

| Command | Description |
|---------|-------------|
| `koda exec [PROMPT]` | Run one prompt and exit, as `codex exec` does. `--json` prints the [client protocol](headless.md#the-client-protocol) on stdout, one JSON-RPC message per line; `--json-deltas` adds streaming text and command output; `-o FILE` writes the last reply to a file. `-` or no prompt reads stdin. Takes every flag above |
| `koda --version` | Print version + kernel sandbox availability (one line, paste-friendly for bug reports) |
