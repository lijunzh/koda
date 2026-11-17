# Sample config.toml

Every setting Koda reads from `~/.config/koda/config.toml`, in one file you
can copy. Everything is commented out: Koda runs without a config file, and
each comment says what happens when a setting is unset. Uncomment the ones you
want.

You rarely need to write this file by hand: `/model` and `/provider` save
your choices in it, and `koda mcp add` adds servers, keeping your comments.
`-c key=value` overrides any key for one run, and `--model`, `--mode` and the
other flags win over both. See [Configuration](./configuration.md) for the
order, and [Environment variables](./environment.md) for what stays in the
environment: API keys and a few others.

A key Koda doesn't know gets a warning at startup naming it and the file
(`Unknown key `model_temperatur` …; Koda ignores it`), so a typo shows. An
unknown key in `-c` stops the run instead.

```toml
# ~/.config/koda/config.toml

# ── Model ──────────────────────────────────────────────────────────────

# The model a new session starts with: an alias that follows the newest
# model in its line (claude-opus, claude-sonnet, claude-haiku,
# claude-fable), or a model id.
# Unset: Claude Sonnet, through Anthropic.
# model = "claude-opus"

# The provider: anthropic, openai, gemini, groq, grok, deepseek, mistral,
# minimax, openrouter, together, fireworks, lm-studio, ollama, vllm or
# llama-cpp. Unset: the model alias's provider, else anthropic.
# model_provider = "anthropic"

# Reasoning effort. Claude: low, medium, high, xhigh or max. Other servers
# get it as `reasoning_effort`, which they may ignore.
# Unset: the level Claude Code uses for the model.
# model_reasoning_effort = "high"

# The context window in tokens, for a server that doesn't report its own.
# Unset: what the server reports, else Koda's table for the model.
# model_context_window = 131072

# What the model takes besides text, over the server's answer and Koda's
# table: for a local vision model whose server doesn't say. Values: "text",
# "image", "pdf".
# Unset: what the server reports, else Koda's table (text only for an
# unknown model).
# model_input_modalities = ["text", "image"]

# The most tokens one reply may use.
# Unset: Koda's default for the model; 32,768 for a local server.
# model_max_output_tokens = 32768

# Sampling temperature, 0.0 to 2.0. Unset: the model's default.
# model_temperature = 0.2

# A thinking budget in tokens, for Gemini models only; Claude uses
# model_reasoning_effort.
# model_thinking_budget = 8192

# ── Local model (koda --oss) ───────────────────────────────────────────

# The local provider --oss uses: llama-cpp (Koda starts and stops its
# server), lm-studio or ollama (Koda connects to the one running).
# Unset: llama-cpp.
# oss_provider = "llama-cpp"

# The model --oss runs on llama.cpp: a Hugging Face <repo>:<quant>, or the
# path of a .gguf file. Unset: Qwen3.8-Flash-Next on a Mac with 128 GB,
# else (Linux included) the 1 GB LFM3-1.7B quick try.
# oss_model = "ornith-ai/Ornith-1.5-35B-A3B-GGUF:Q4_K_M"

# macOS: refuse to start a model while the GPU is busy (another model
# server, a model loaded in LM Studio, or the GPU in use). Unset: true.
# oss_check_gpu = false

# ── Sessions ───────────────────────────────────────────────────────────

# The trust mode a new session starts in: auto, safe or plan.
# Unset: auto.
# mode = "safe"

# The agent sessions run: a built-in (default, task, explore) or the name
# of a JSON file in .koda/agents/ or ~/.config/koda/agents/.
# Unset: default.
# agent = "default"

# Compact as soon as a turn's last reply is in, once this share (1-100) of
# the context window is used. Unset or 0: off; Koda still compacts at 85%
# before a model call.
# model_post_turn_compact_threshold_percent = 85

# false sends an @-mentioned file's path only, for the model to read
# itself, as Codex does. Unset: true, the file's contents are sent.
# file_mentions = false

# Have the model end its commit messages with
# `Co-authored-by: Koda <noreply@lijunzhu.info>` and add a "Generated with
# Koda" line to its pull requests, as Codex does. Unset: true.
# commit_attribution = false

# Tokens one turn may spend, sub-agents included: input not read from the
# cache, plus output. Near it the model is told to wrap up; at it the turn
# stops. At least 20000. Unset: no limit.
# turn_token_budget = 200000

# Also write the log to <log_dir>/koda.log, at RUST_LOG's level. "~/" is
# your home folder; a relative path is under ~/.config/koda/.
# Unset: only the log database (see the Debugging chapter).
# log_dir = "logs"

# ── Providers ──────────────────────────────────────────────────────────
# One table per provider id, with its endpoint and its retries.

# [model_providers.llama-cpp]
# The server's address. Unset: the provider's default
# (llama-cpp: http://localhost:8080/v1, lm-studio: http://localhost:1234/v1).
# base_url = "http://127.0.0.1:8080/v1"
# Retries after a request fails with a rate limit, server error or network
# error (at most 100; 0 turns them off). Unset: 4.
# request_max_retries = 4
# Retries for a reply whose stream fails partway (at most 100). Unset: 5.
# stream_max_retries = 5
# Milliseconds a reply may go silent before the connection counts as lost;
# 0 turns the limit off. Unset: 300000 (5 minutes), or 1800000 (30 minutes)
# for a provider without an API key, such as a local server.
# stream_idle_timeout_ms = 1800000

# ── MCP servers ────────────────────────────────────────────────────────
# Every new session starts these; `/mcp reconnect` brings open sessions in
# line. `koda mcp add` writes them for you.

# A server Koda starts as a command (stdio):
# [mcp_servers.playwright]
# command = "npx"
# args = ["-y", "@playwright/mcp"]
# env = { DEBUG = "0" }                       # variables set for it
# cwd = "/path/to/folder"                     # unset: where Koda was started
# enabled_tools = ["navigate", "click", "screenshot"]   # only these, or:
# disabled_tools = ["close"]                  # all but these (enabled_tools wins)
# startup_timeout_sec = 30                    # unset: 30
# tool_timeout_sec = 120                      # unset: 120
# required = true                             # unset: false; true: a session
#                                             # doesn't start without it

# A server over HTTP:
# [mcp_servers.docs]
# url = "https://mcp.example.com/mcp"
# bearer_token_env_var = "DOCS_MCP_TOKEN"     # the variable holding its token
# http_headers = { "X-Team" = "platform" }    # sent with every request

# ── TUI ────────────────────────────────────────────────────────────────

# [tui]
# Colour code blocks and diffs. Unset: true.
# syntax_highlight = false
# Notify when a turn ends, a tool call waits for approval or the model asks
# a question. Unset: true. Or only some of them:
# notifications = ["approval-requested", "user-input-requested"]
# How: a desktop notification (OSC 9) in Ghostty, iTerm2, Kitty, Warp and
# WezTerm, the bell elsewhere. Unset: "auto". Or "osc9", "osc777", "bel".
# notification_method = "bel"
# When: only while the terminal isn't focused. Unset: "unfocused". Or "always".
# notification_condition = "always"

# ── Sandbox ────────────────────────────────────────────────────────────

# [sandbox]
# macOS: let commands listen on any local port and reach any port on
# localhost, not only the servers Koda started. Unset: false.
# allow_local_binding = true
```

## A starting point

Most people need two or three lines. For Claude with a key in your
environment, nothing at all. For a local model on `llama-server`:

```toml
model_provider = "llama-cpp"

[model_providers.llama-cpp]
base_url = "http://127.0.0.1:8080/v1"
```

To ask before every change, and to see files you mention only by path:

```toml
mode = "safe"
file_mentions = false
```
