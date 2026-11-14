# Providers & model aliases

## Supported providers

| Provider name | `--provider` value | API key env var | Default model | Needs key |
|---|---|---|---|---|
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-5-5 | ✓ |
| OpenAI | `openai` | `OPENAI_API_KEY` | gpt-6-astra | ✓ |
| Google Gemini | `gemini` | `GEMINI_API_KEY` | gemini-flash-latest | ✓ |
| Groq | `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | ✓ |
| Grok / xAI | `grok` | `XAI_API_KEY` | grok-4.7 | ✓ |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | ✓ |
| Mistral | `mistral` | `MISTRAL_API_KEY` | mistral-large-latest | ✓ |
| MiniMax | `minimax` | `MINIMAX_API_KEY` | minimax-text-01 | ✓ |
| OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-sonnet-5.5 | ✓ |
| Together AI | `together` | `TOGETHER_API_KEY` | Llama-3.3-70B-Instruct-Turbo | ✓ |
| Fireworks AI | `fireworks` | `FIREWORKS_API_KEY` | llama-v3p3-70b-instruct | ✓ |
| LM Studio | `lm-studio` | — | auto-detect | ✗ |
| Ollama | `ollama` | — | auto-detect | ✗ |
| vLLM | `vllm` | — | auto-detect | ✗ |
| llama.cpp | `llama-cpp` | — | auto-detect | ✗ |

Gemini gets far less testing than Claude, so expect some bugs. See its
[known limitations](https://github.com/lijunzh/koda/issues/1473).

Local providers (LM Studio, Ollama, vLLM, llama.cpp) are auto-detected on
first run and require no API key. The model is discovered from the running
server. The names are shortcuts for a default URL (`lm-studio` is
`http://localhost:1234/v1`, `llama-cpp` is `http://localhost:8080/v1`).

Two guards apply to local models only, since a local model can loop inside
one reply and these servers set no limit of their own:

- **A reply that repeats itself is stopped.** When a reply's text or
  reasoning starts repeating the same passage, Koda stops it, drops it and
  asks the model to carry on. Five such replies in a row end the turn. In
  Koda's evals, Ornith once spent a whole 20-minute task repeating one
  paragraph 900 times; the guard stops that within a few repeats.
- **Each reply is capped at 32,768 tokens** unless you set `--max-tokens`.
  A reply cut off at the cap is told so and asked to continue, or to reason
  more briefly if it was cut off before answering.

## Any OpenAI-compatible server

A server that speaks the OpenAI chat-completions API works with no provider
entry: point Koda at its `/v1` URL with `--base-url`, or set it once as the
provider's `base_url` in your config file.

```bash
llama-server -m model.gguf -c 32768 --port 8080
koda --base-url http://127.0.0.1:8080/v1
# or, in ~/.config/koda/config.toml:
#   [model_providers.llama-cpp]
#   base_url = "http://127.0.0.1:8080/v1"
```

- Koda lists the server's models and picks the loaded one, and reads the
  context window from llama-server's `/props` (LM Studio and Ollama report
  theirs through their own endpoints). For a server that reports none, Koda
  prints the size it assumed, and if the server rejects a request with a
  stated limit (llama.cpp, vLLM, OpenAI), Koda adopts that limit and says so.
- **Give llama-server enough context.** Koda's system prompt and tool schemas
  are about 8K tokens, so start it with `-c 16384` or more. The llama-server
  default of 4096 fails with a 400.
- Inside the TUI, `/provider http://127.0.0.1:8080/v1` switches to a URL and
  remembers it for the next session, as `[model_providers.llama-cpp] base_url`
  in `~/.config/koda/config.toml`. You can also write that line yourself. A
  URL passed with `--base-url` applies to that run only.
- [mistral.rs](https://github.com/EricLBuehler/mistral.rs) serves the same
  API, so the same route should work. It hasn't been tested.

## Model aliases

Aliases let you switch models without memorising exact IDs. They're shown
in the `/model` picker and accepted by `/model`, `--model` and `model` in the config file.
On the command line an alias also picks its provider, unless `--provider` or
`--base-url` names a different one; then the name goes to that provider as
given. A Claude alias resolves to the newest model in its line, as `/model`
does.

| Alias | Provider | Exact model ID |
|-------|----------|----------------|
| `gemini-flash-lite` | Gemini | `gemini-flash-lite-latest` |
| `gemini-flash` | Gemini | `gemini-flash-latest` |
| `gemini-pro` | Gemini | `gemini-pro-latest` |
| `claude-haiku` | Anthropic | `claude-haiku-5-5` |
| `claude-sonnet` | Anthropic | `claude-sonnet-5-5` |
| `claude-opus` | Anthropic | `claude-opus-5-5` |
| `claude-fable` | Anthropic | `claude-fable-5-2` |
| `local` | LM Studio | auto-detect at runtime |

The four Claude aliases follow the Anthropic Models API: when an API key is
set, Koda asks at startup for the newest model in each line and uses it, with
a 5-second cap. Picking Anthropic in `/provider` offers those same newest
models, not every model the API lists; an older one can still be typed, and
gets a warning that Koda is built for the current models. When a line's newest
model is one this Koda doesn't support yet, the TUI says so at startup, with a
link to ask for it in an issue; the model still runs, without a tuned effort.

A model you pick from that list is remembered as its alias, so you move to the
next release on your own. One you type by hand (`/model claude-opus-5-5`)
stays as typed, and once its line has a newer model, Koda reminds you at
startup: "You're on claude-opus-5-5; claude-opus-6 is newer. /model
claude-opus follows the newest." A resumed session runs on your current
choice, not the model it was started with. The ids above are
the offline fallback, used with no key, no network or an API error. A model
picked by alias is remembered as the alias, so a newer release is picked up on
the next start.

You can also use **any literal model ID** your provider supports — aliases
are just shortcuts. `koda --model gpt-4o-mini` or `/model o3` both work.

## HTTP timeouts and retries

All providers use a shared HTTP client with the following defaults. Each
setting is per provider, under `[model_providers.<id>]` in your config file,
with Codex's names:

| Setting | Default | Config key | Description |
|---------|---------|------------|-------------|
| Connect timeout | 30 s | (fixed) | Time allowed to establish the TCP/TLS connection |
| Read timeout | 300 s (5 min); 1,800 s (30 min) for a provider with no API key | `stream_idle_timeout_ms` | Time allowed between bytes from the server (per-byte, not total), in milliseconds; `0` turns it off |
| Request retries | 4 | `request_max_retries` | Retries after a request fails with a rate limit, server error or network error; at most 100, `0` turns them off |
| Stream retries | 5 | `stream_max_retries` | Retries for a reply whose stream fails partway; at most 100, `0` turns them off |

The read timeout is **per-byte, not total**. A long streaming response is
fine as long as bytes keep arriving — the timer resets on each chunk.
This means slow networks or chatty SSE streams won't get murdered mid-turn,
but a stalled connection (server hung after last byte) will fail fast.

### When to tune these

- **Long-running model on a flaky link?** Raise `stream_idle_timeout_ms` to
  600000 or more. Read-phase timeouts manifest as a partial response cut
  short partway through generation. (Koda also retries transient network
  errors, waiting 1 second before the first retry and twice as long before
  each next one, up to 32; the retry settings above set how many.)
- **Provider with no API key (LM Studio, Ollama, vLLM, llama.cpp, or a
  self-hosted server)?** The default is 30 minutes, because such a server
  sends nothing until it has read the whole prompt, and a 150K-token prompt
  can take a local model more than five minutes. A server that has died
  refuses the connection at once, so the long wait only covers one that is
  alive but stuck. Lower `stream_idle_timeout_ms` if you'd rather fail fast.
- **Local server whose errors won't go away on a retry?** LM Studio answers a
  prompt too large for its loaded context window with a bare 500, which Koda
  retries like any server error: four more tries and 15 seconds before the
  turn ends. Set `request_max_retries = 0` to see the error at once, or `1`
  to still ride out a single hiccup. `stream_max_retries` does the same for a
  reply that breaks off partway.

### Example

```toml
# ~/.config/koda/config.toml
[model_providers.lm-studio]
stream_idle_timeout_ms = 600000
request_max_retries = 1
```

Or for one run: `koda -c model_providers.lm-studio.request_max_retries=0`.
