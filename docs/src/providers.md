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
