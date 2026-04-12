# CLI reference

## Flags

| Flag | Env var | Description |
|------|---------|-------------|
| `-p`, `--prompt <PROMPT>` | | Run a single prompt and exit (headless). Use `"-"` for stdin |
| `<PROMPT>` (positional) | | Same as `-p` — `koda "fix the bug"` works |
| `-a`, `--agent <NAME>` | | Agent definition to use (JSON in `agents/`, default: `default`) |
| `-s`, `--resume <ID>` | | Resume a previous session by ID prefix |
| `--model <NAME>` | `KODA_MODEL` | Model name or alias (e.g. `claude-sonnet`, `gemini-flash`) |
| `--provider <NAME>` | `KODA_PROVIDER` | LLM provider (`anthropic`, `gemini`, `openai`, `ollama`, …) |
| `--base-url <URL>` | `KODA_BASE_URL` | Override the provider's API base URL |
| `--max-tokens <N>` | | Maximum output tokens |
| `--temperature <F>` | | Sampling temperature (0.0–2.0) |
| `--thinking-budget <N>` | | Anthropic extended thinking budget (tokens) |
| `--reasoning-effort <L>` | | OpenAI reasoning effort (`low`, `medium`, `high`) |
| `--sandbox <MODE>` | `KODA_SANDBOX` | Bash tool sandbox: `none` (default), `project` (restrict writes to project dir + `/tmp`), or `strict` (project + deny reads of credential dirs: `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.azure`, `~/.config/gcloud`) |
| `--output-format <FMT>` | | Headless output format: `text` (default) or `json` |
| `--project-root <DIR>` | | Project root (defaults to cwd) |

## Subcommands

| Command | Description |
|---------|-------------|
| `koda server --stdio` | Start ACP server over stdin/stdout (for editors) |
| `koda server --port <N>` | WebSocket ACP server on port N (not yet implemented) |
