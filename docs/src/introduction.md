# Koda 🐻

Koda is a terminal-native AI coding agent. It runs locally, keeps all
data on your machine, and connects to any LLM provider you choose.

## Modes at a glance

| Mode | How to invoke | Best for |
|------|--------------|----------|
| **Interactive TUI** | `koda` (no args) | Long sessions, iterative coding |
| **Headless** | `koda "prompt"` or `echo … \| koda` | Scripts, CI, one-shot tasks |
| **ACP server** | `koda server --stdio` | Editor plugins (VS Code, Zed, …) |

## Quick start

```bash
# 1. Open the interactive TUI
koda

# 2. Ask something at the prompt
#    > explain why the auth tests are failing

# 3. Type /help inside for keybindings and commands
```

First run triggers onboarding: Koda looks for a running local model
(LM Studio, Ollama) and falls back to prompting for a cloud API key.

## What's next

- Jump straight to the [CLI reference](./cli-reference.md) for all flags
- Read about [headless mode](./headless.md) for scripting and CI use
- Explore [slash commands](./commands.md) for everything you can do in a session
- See [providers & model aliases](./providers.md) to connect your preferred LLM
