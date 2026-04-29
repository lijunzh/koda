# Providers & model aliases

## Supported providers

| Provider name | `--provider` value | API key env var | Default model | Needs key |
|---|---|---|---|---|
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-6 | ✓ |
| OpenAI | `openai` | `OPENAI_API_KEY` | gpt-4o | ✓ |
| Google Gemini | `gemini` | `GEMINI_API_KEY` | gemini-flash-latest | ✓ |
| Groq | `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | ✓ |
| Grok / xAI | `grok` | `XAI_API_KEY` | grok-3 | ✓ |
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | ✓ |
| Mistral | `mistral` | `MISTRAL_API_KEY` | mistral-large-latest | ✓ |
| MiniMax | `minimax` | `MINIMAX_API_KEY` | minimax-text-01 | ✓ |
| OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-3.5-sonnet | ✓ |
| Together AI | `together` | `TOGETHER_API_KEY` | Llama-3.3-70B-Instruct-Turbo | ✓ |
| Fireworks AI | `fireworks` | `FIREWORKS_API_KEY` | llama-v3p3-70b-instruct | ✓ |
| LM Studio | `lm-studio` | — | auto-detect | ✗ |
| Ollama | `ollama` | — | auto-detect | ✗ |
| vLLM | `vllm` | — | auto-detect | ✗ |

Local providers (LM Studio, Ollama, vLLM) are auto-detected on first run
and require no API key. The model is discovered from the running server.

## Model aliases

Aliases let you switch models without memorising exact IDs. They're shown
in the `/model` picker and accepted by `--model` and `/model`.

| Alias | Provider | Exact model ID |
|-------|----------|----------------|
| `gemini-flash-lite` | Gemini | `gemini-flash-lite-latest` |
| `gemini-flash` | Gemini | `gemini-flash-latest` |
| `gemini-pro` | Gemini | `gemini-pro-latest` |
| `claude-haiku` | Anthropic | `claude-haiku-4-5-20251001` |
| `claude-sonnet` | Anthropic | `claude-sonnet-4-6` |
| `claude-opus` | Anthropic | `claude-opus-4-6` |
| `local` | LM Studio | auto-detect at runtime |

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
