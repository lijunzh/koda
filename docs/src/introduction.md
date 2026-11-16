# Koda 🐻

Koda is an AI coding agent for the terminal, on macOS and Linux. It reads and
edits your project, runs commands in a [kernel sandbox](./sandbox.md), and
works with Claude, OpenAI-compatible servers and local models.

**Where your data goes:** Koda keeps its sessions, settings and keys on your
machine, in `~/.config/koda`. What you send the model, including the files it
reads, goes to the provider you choose; with a local server, the conversation
stays on your machine. Koda has no telemetry. See [Privacy &
data](./privacy.md).

## Ways to run it

| Mode | How to invoke | Best for |
|------|--------------|----------|
| **Interactive TUI** | `koda`, or `koda "prompt"` to start with one | Long sessions, iterative coding |
| **Headless** | `koda exec "prompt"` or `echo … \| koda exec` | Scripts, CI, one-shot tasks |
| **Server** | `koda serve --stdio` | A GUI, an editor or a script driving Koda over its [client protocol](./serve.md) |

## Quick start

[Install Koda](./installation.md), then:

```bash
# 1. Open the interactive TUI
koda

# 2. Ask something at the prompt
#    > explain why the auth tests are failing

# 3. Type /help inside for keybindings and commands
```

The first run opens the provider picker: choose a provider and enter its API
key, or give a local server's address. A first run that already names one
skips it: with `--provider`, `--base-url` (Koda takes the provider from the
address) or a model alias such as `--model claude-opus`.

## What's next

- Coming from 0.6? Read [Upgrading to 0.7](./upgrading-0.7.md)
- The [CLI reference](./cli-reference.md) lists every flag
- [Trust modes](./approval.md) decide what Koda may do without asking
- [Headless mode](./headless.md) is for scripts and CI
- [Slash commands](./commands.md) list everything you can do in a session
- [Providers & model aliases](./providers.md) connect your preferred model
