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

## Any OpenAI-compatible server

A server that speaks the OpenAI chat-completions API works with no provider
entry and no config file: point Koda at its `/v1` URL with `--base-url` or
`KODA_BASE_URL`.

```bash
llama-server -m model.gguf -c 32768 --port 8080
koda --base-url http://127.0.0.1:8080/v1
# or: export KODA_BASE_URL=http://127.0.0.1:8080/v1
```

- Koda lists the server's models and picks the loaded one, and reads the
  context window from llama-server's `/props` (LM Studio and Ollama report
  theirs through their own endpoints). For a server that reports none, Koda
  prints the size it assumed, and if the server rejects a request with a
  stated limit (llama.cpp, vLLM, OpenAI), Koda adopts that limit and says so.
- **Give llama-server enough context.** Koda's system prompt and tool schemas
  are about 8K tokens, so start it with `-c 16384` or more. The llama-server
  default of 4096 fails with a 400.
- Inside the TUI, `/provider http://127.0.0.1:8080/v1` switches to a URL, and
  a URL passed with `--base-url` or `KODA_BASE_URL` is remembered for the
  next session. Headless runs (`-p`) never save anything.
- [mistral.rs](https://github.com/EricLBuehler/mistral.rs) serves the same
  API, so the same route should work. It hasn't been tested.

## Model aliases

Aliases let you switch models without memorising exact IDs. They're shown
in the `/model` picker and accepted by `/model`. `--model` and `KODA_MODEL` take
an exact ID.

| Alias | Provider | Exact model ID |
|-------|----------|----------------|
| `gemini-flash-lite` | Gemini | `gemini-flash-lite-latest` |
| `gemini-flash` | Gemini | `gemini-flash-latest` |
| `gemini-pro` | Gemini | `gemini-pro-latest` |
| `claude-haiku` | Anthropic | `claude-haiku-5-5` |
| `claude-sonnet` | Anthropic | `claude-sonnet-5-5` |
| `claude-opus` | Anthropic | `claude-opus-5-5` |
| `local` | LM Studio | auto-detect at runtime |

The three Claude aliases follow the Anthropic Models API: when an API key is
set, the first time you open `/model` (or pick an alias) Koda asks for the
newest model in each line and uses it, with a 5-second cap. The ids above are
the offline fallback, used with no key, no network or an API error. A model
picked by alias is remembered as the alias, so a newer release is picked up on
the next start.

You can also use **any literal model ID** your provider supports — aliases
are just shortcuts. `koda --model gpt-4o-mini` or `/model o3` both work.

## HTTP timeouts

All providers use a shared HTTP client with the following timeout defaults:

| Setting | Default | Env override | Description |
|---------|---------|--------------|-------------|
| Connect timeout | 30 s | `KODA_CONNECT_TIMEOUT_SECS` | Time allowed to establish the TCP/TLS connection |
| Read timeout | 300 s (5 min) | `KODA_READ_TIMEOUT_SECS` | Time allowed between bytes from the server (per-byte, not total) |

The read timeout is **per-byte, not total**. A long streaming response is
fine as long as bytes keep arriving — the timer resets on each chunk.
This means slow networks or chatty SSE streams won't get murdered mid-turn,
but a stalled connection (server hung after last byte) will fail fast.

### When to tune these

- **Behind a slow corporate proxy?** Bump `KODA_CONNECT_TIMEOUT_SECS` to
  60 or 90. Connection-phase timeouts often manifest as "request timed out"
  with no usage data, which is the giveaway.
- **Long-running model on a flaky link?** Bump `KODA_READ_TIMEOUT_SECS` to
  600+. Read-phase timeouts manifest as a partial response cut short
  partway through generation. (Note: koda also auto-retries transient
  network errors up to 5 times with exponential backoff; see `is_network_transient_error`.)
- **Local provider (Ollama, LM Studio, vLLM) and you want fail-fast?**
  Drop `KODA_READ_TIMEOUT_SECS` to 30 — local models that hang are usually
  truly hung, not slow.

### Example

```bash
KODA_CONNECT_TIMEOUT_SECS=60 KODA_READ_TIMEOUT_SECS=300 koda
```
