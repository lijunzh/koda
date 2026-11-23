# Environment variables

Koda's settings live in [`config.toml`](./config-sample.md), not the
environment, as Codex's do. The environment holds what belongs to you or your
machine rather than to Koda: API keys, where Koda's folder is, your proxy,
and logging. Koda 0.6's setting variables (`KODA_MODEL`, `KODA_MODE`, …) are
ignored with a warning; [Upgrading to
0.7](./upgrading-0.7.md#settings-live-in-configtoml-not-the-environment)
lists what replaced each.

## API keys

| Provider | Variable |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Grok / xAI | `XAI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| MiniMax | `MINIMAX_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Together AI | `TOGETHER_API_KEY` |
| Fireworks AI | `FIREWORKS_API_KEY` |
| LM Studio, Ollama, vLLM, llama.cpp | `KODA_API_KEY`, only for a server that requires one (vLLM started with `--api-key`, say) |
| Brave Search, for `WebSearch` on models other than Claude | `BRAVE_API_KEY`. Without it `WebSearch` uses DuckDuckGo, which often answers with a bot check |

You don't need to set these: `/key` in the TUI saves a key in Koda's
database, a model provider's or Brave Search's, and Koda loads it at
startup. **A variable you set wins over a saved key** when Koda starts; a key
you type into `/key` is used for the rest of that run. Keys never go in `config.toml`, and the commands Koda runs never
see them (see [Commands Koda runs](#commands-koda-runs)).

An MCP server's token stays in a variable too: its `bearer_token_env_var`
names which one ([MCP servers](./mcp.md)).

## Koda's folder

| Variable | Effect |
|---|---|
| `XDG_CONFIG_HOME` | Koda's folder is `$XDG_CONFIG_HOME/koda`. Unset: `~/.config/koda` |
| `HOME` | Your home folder, for `~/.config/koda` and for `~/` in paths |

Everything Koda keeps is in that folder: `config.toml`, the database with
sessions, keys and logs, your `AGENTS.md`, notes, agents and skills. Pointing
`XDG_CONFIG_HOME` at an empty folder gives a run with no saved settings,
which is how Koda's own tests run.

## Network

| Variable | Effect |
|---|---|
| `HTTPS_PROXY`, `HTTP_PROXY` (or lowercase) | Koda's requests to model providers go through this proxy |
| `PROXY_USER`, `PROXY_PASS` | Basic authentication for that proxy, when its URL carries no credentials |
| `KODA_ACCEPT_INVALID_CERTS=1` | Turn off certificate checks for a provider on `localhost` or `127.0.0.1` only, for a local server with a self-signed certificate. For any other address it is ignored, with a warning |

`KODA_ACCEPT_INVALID_CERTS` is the one setting still read from the
environment; it has no `config.toml` key.

## Logging and debugging

| Variable | Effect |
|---|---|
| `RUST_LOG` | What Koda logs, by crate and level: `RUST_LOG=debug`, or `RUST_LOG=koda_providers=trace` to log each request body sent to the model |
| `RUST_BACKTRACE=1` | A backtrace when Koda crashes |

See [Debugging a run](./debugging.md) and
[Troubleshooting](./troubleshooting.md).

## The terminal

| Variable | Effect |
|---|---|
| `SSH_CONNECTION` | Koda is running over SSH: copying goes to your local terminal with an escape sequence (OSC 52) rather than the remote machine's clipboard, and `Ctrl+V` can't paste an image |
| `TMUX` | Inside tmux, copying also goes into tmux's paste buffer, which passes it on to your terminal (tmux 3.2 or later) |

## Commands Koda runs

Commands run by the model don't inherit your environment. Each gets a fixed
set of your variables: identity and locale (`HOME`, `USER`, `SHELL`, `LANG`,
`LC_*`), `PATH`, `TERM`, the temporary folder, and for some tools their own
(`CARGO_HOME` and `RUSTUP_HOME` for `cargo`, the `GIT_*` identity for `git`,
`VIRTUAL_ENV` and `PYTHONPATH` for Python). API keys, tokens and other
secrets aren't among them, so a command such as `env` or `echo
$ANTHROPIC_API_KEY` can't leak one into the conversation. See
[Sandbox](./sandbox.md).
