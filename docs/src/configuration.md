# Configuration

## Precedence

When multiple sources specify the model, provider, or API key, the
**highest-priority source wins**:

```text
1. CLI flags          --model, --provider, --base-url, --mode, …  (highest)
       ↓
2. -c key=value       this run only
       ↓
3. Config file        ~/.config/koda/config.toml, which /model and /provider write
       ↓
4. Built-in defaults  Claude Sonnet via Anthropic                 (lowest)
```

Settings don't come from environment variables, as Codex's don't. One that
an earlier version read is ignored, with a warning at startup naming its
replacement; [Upgrading to 0.7](./upgrading-0.7.md#settings-live-in-configtoml-not-the-environment)
lists them.

API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …) are
never in the config file. Keys saved with `/key` are kept in Koda's database
and injected at startup, but a key already in the shell environment takes
precedence and is never overwritten.

```bash
# Per-call override (doesn't change saved config)
koda exec --model o3 "review auth.rs"

# One run on another setting, without touching config.toml
koda exec -c model_reasoning_effort=high "review auth.rs"

# CI / GitHub Actions
ANTHROPIC_API_KEY=${{ secrets.ANTHROPIC_KEY }} koda exec "check types"
```

## The config file

[Sample config.toml](./config-sample.md) has every key in one commented file to copy, and [Environment variables](./environment.md) lists what stays in the environment.

`~/.config/koda/config.toml` holds the settings a new session starts with,
as Codex keeps `~/.codex/config.toml`. `/model` (Enter, not `s`, which
changes only the session), `/provider` and `koda mcp` write it, and you can
edit it by hand:

```toml
# The model a new session starts with: an alias such as claude-opus follows
# its line to the newest release; an id stays as written.
model_provider = "anthropic"
model = "claude-opus"

# A provider's endpoint, used whenever a session runs on that provider.
[model_providers.lm-studio]
base_url = "http://192.168.1.20:1234/v1"

# MCP servers every new session starts (see MCP Servers).
[mcp_servers.notes]
command = "notes-mcp"
args = ["--quiet"]
```

Every key, in Codex's names where Codex has one:

| Key | What it sets |
|-----|--------------|
| `model_provider`, `model` | The provider and model a new session starts with |
| `model_reasoning_effort` | The reasoning effort, as `--effort` gives it |
| `model_context_window` | The context window to assume, in tokens, as `--max-context-tokens` pins it |
| `model_max_output_tokens` | The most tokens a reply may use, as the deprecated `--max-tokens` gives it |
| `model_temperature` | The sampling temperature (0.0–2.0), as the deprecated `--temperature` gives it |
| `model_thinking_budget` | A thinking budget in tokens, for Gemini models, as the deprecated `--thinking-budget` gives it |
| `agent` | The agent sessions run, as `--agent` names it: a built-in, or a JSON file in `.koda/agents/` or `~/.config/koda/agents/` (default: `default`) |
| `model_post_turn_compact_threshold_percent` | Compact after a turn's final reply once this share of the window is used (1–100; 0 turns it off) |
| `file_mentions` | `false` sends an `@` mention's path only, for the model to read, as Codex does; on by default, sending its contents as Claude Code does (see [File & image attachments](./attachments.md#mentioning-files)) |
| `goals` | `false` turns goals off: the model loses its goal tools and Koda starts no turn on a goal by itself. On by default, as Codex's (see [`/goal`](./commands.md#goal)) |
| `background_time_limit_minutes` | How long a background command may run in a session nobody attends ([`koda exec`](./headless.md#background-commands), or a client with `approvalPolicy: never`); the model's `timeout` sets another, up to 2 hours. Koda stops a command at its limit and tells the model. 10 by default, as `claude -p`'s; `0` takes the limit off |
| `max_goal_token_budget` | The most tokens a goal may use: a goal set without a budget gets this one, and a budget over it is refused, as Codex's setting. Unset: no limit (see [`/goal`](./commands.md#goal)) |
| `goal_checkin_minutes` | How long a goal waits on running background commands before Koda checks in on them; later check-ins wait twice as long, up to four times this, at most three until your next message. 30 by default, as Claude Code's; `0` turns them off (see [`/goal`](./commands.md#goal)) |
| `goal_judge` | Who decides a goal is achieved: `"self"`, the working model, as in Codex (the default), or `"evaluator"`, a check after each turn, as in Claude Code (see [Who decides it's done](./commands.md#who-decides-its-done)) |
| `commit_attribution` | `false` stops the model adding Koda's attribution to the commits and pull requests it makes. On by default, as Codex's: commit messages end with `Co-authored-by: Koda <noreply@lijunzhu.info>`, and pull requests carry `Generated with [Koda](https://github.com/lijunzh/koda).` |
| `review_model` | The model [`/review`](./commands.md#review) runs on, on the session's provider, as Codex's `review_model`. The session's own model when unset |
| `developer_instructions` | Text added to the end of the system prompt, as Codex's `developer_instructions`. Most useful per session through `session/start`'s `config`, which is how a [side conversation](./tui.md#side-conversations) gets its rules |
| `turn_token_budget` | Tokens one turn may spend, sub-agents and compaction included, counted as Codex counts a goal's (input not read from the cache, plus output). At least 20,000; no limit when unset. See [Limiting what a turn spends](#limiting-what-a-turn-spends) |
| `approval_wait_timeout_secs` | How long an approval, a question or the loop cap waits for a client on the [shared server](./serve.md#the-shared-server) before Koda answers no (default 86400, a day) |
| `model_input_modalities` | What the model takes besides text, over what a local server reports and Koda's table: `["text", "image"]` for a vision model the server doesn't report; `pdf` is the third value (see [File & image attachments](./attachments.md#images)) |
| `mode` | The [trust mode](./approval.md) a new session starts in: `auto`, `safe` or `plan` |
| `oss_provider` | The local provider `--oss` uses: `llama-cpp` (default; Koda starts and stops it), `lm-studio` or `ollama` ([Local models](./local-models.md#one-command-koda---oss)) |
| `oss_model` | The model `--oss` runs on llama.cpp: a Hugging Face `<repo>:<quant>` or a `.gguf` path; unset, Qwen3.8-Flash-Next on a Mac with 128 GB, else the 1 GB quick try |
| `oss_check_gpu` | `false` lets `--oss` start a model while the GPU is busy (macOS); on by default |
| `[model_providers.<id>]` | Per provider: `base_url`, `request_max_retries`, `stream_max_retries`, `stream_idle_timeout_ms` (0 turns the limit off) |
| `[mcp_servers.<name>]` | An MCP server every new session starts |
| `[tui] syntax_highlight` | `false` shows code and diffs without colour. Read by the TUI from its own machine's file |
| `[tui] notifications` | Notify when a turn ends or Koda waits for you: `true` (unset), `false`, or the events, from `agent-turn-complete`, `approval-requested`, `user-input-requested` (see [TUI](./tui.md#notifications)) |
| `[tui] notification_method` | `auto` (unset): a desktop notification in Ghostty, iTerm2, Kitty, Warp and WezTerm, the bell elsewhere; or `osc9`, `osc777`, `bel` |
| `[tui] notification_condition` | `unfocused` (unset): only while the terminal isn't focused; or `always` |
| `log_dir` | Also write the log to `<log_dir>/koda.log`, at `RUST_LOG`'s level, as Codex's `log_dir` does; relative to `~/.config/koda/`. Unset, the log database is the only log (see [Debugging a run](./debugging.md#the-log)) |
| `[sandbox] allow_local_binding` | `true` lets sandboxed commands on macOS use any local port (see [Sandbox](./sandbox.md#when-a-command-needs-to-leave-the-sandbox)) |

**`-c key=value` overrides a key for one run,** as Codex's `-c` does, and is
never saved: `koda -c model=claude-opus`, or
`koda exec -c model_providers.lm-studio.base_url=http://127.0.0.1:1234/v1 "…"`.
The value is read as TOML (`5`, `true`, `"text"`), and as plain text when it
isn't, and replaces what is at its key, a whole table included. A key Koda
doesn't know stops the run.

**Most flags are `-c` values,** as Codex's are: `--effort` is
`model_reasoning_effort`, `--max-tokens` `model_max_output_tokens`,
`--temperature` `model_temperature`, `--thinking-budget`
`model_thinking_budget`, `--max-context-tokens` `model_context_window`,
`--mode` `mode` and `--agent` `agent`; `--mcp-config` gives
`mcp_servers.<name>` values. A flag wins over a `-c` value for the same key.
`--model`, `--provider` and `--base-url` choose the session's model instead,
as Codex's TUI passes its model to the session it starts.

**A value given for the run** (a flag or `-c`) wins over the agent's JSON.
The file's own value fills in only what the agent's JSON leaves unset. An
effort or thinking budget given for the run that the Claude model it starts
with doesn't take stops the run, so the command can be fixed. A later session
on another model (`/provider`) gets a level that model takes instead, as
Codex falls back when a switch lands on a model without it.

- **A hand edit applies to the next session,** without a restart. Run
  `/mcp reconnect` for open sessions to take MCP server changes.
- **Koda keeps your comments and layout** when it writes the file.
- **A file that isn't valid stops Koda at startup,** naming the line and
  column, so a typo can't quietly change what runs.
- **A key Koda doesn't know is reported at startup** and otherwise ignored.
  So is an MCP server it can't start.
- **The file is readable by you only** (mode 0600), since an MCP server's
  `env` can hold a token. API keys never go in it.
- **Settings move in once.** Earlier versions kept the model choice and MCP
  servers in the database; the first start of this version writes them into
  the file, unless it already sets them, and deletes them from the database.

The file is per machine, so a laptop and a desktop can point at different
local servers.

## Context window

Koda needs the model's context window to show the footer percentage, warn
before a request overflows, and auto-compact. It takes the first of these:

```text
1. --max-context-tokens N (or -c model_context_window=N)
2. "max_context_tokens" in the agent JSON
3. model_context_window in config.toml
4. What the server reports (LM Studio, llama-server, Ollama, Anthropic, Gemini)
5. Koda's built-in model table
```

Use 1 or 3 when the server loaded a different size than Koda assumed, or can't
say (an OpenAI-compatible engine such as `rec` falls to the table, which
assumes 128,000 for most local models). A pinned value also stays when you
switch models with `/model`. It must be a positive whole number; `0` or
text stops Koda at startup with an error rather than being ignored.

```bash
koda exec --max-context-tokens 65536 "say hi" --provider lm-studio --base-url http://127.0.0.1:11500/v1
```

## Config files

Everything lives in `~/.config/koda/`:

| Path | Content |
|------|---------|
| `config.toml` | Your settings: the model choice, provider endpoints, MCP servers (see [The config file](#the-config-file)) |
| `db/koda.db` | SQLite — sessions, messages, API keys |
| `db/logs_1.db` | SQLite — the log, by session, read with `koda-logs` (see [Debugging a run](./debugging.md#the-log)) |
| `history.jsonl` | What you typed in the TUI, for ↑ / ↓: the newest 500 entries, one JSON object per line, readable by you only |
| `agents/` | Global custom agent JSON definitions |
| `skills/` | Global custom skill markdown files |
| `AGENTS.md` | Your instructions for every project (sent with every request) |
| `memories/` | Koda's notes: `MEMORY.md` for all projects, `<project>/MEMORY.md` for each |

Project-level overrides live in `.koda/` at your project root and take
priority over global config:

| Path | Content |
|------|---------|
| `.koda/agents/` | Project-specific agent definitions |
| `.koda/skills/` | Project-specific skills |
| `AGENTS.md` | Project instructions, in each folder from the git root down (`CLAUDE.md` where a folder has none) |

## Display

Koda's TUI colours code and diffs by default. Turn it off for a monochrome
theme or a terminal where RGB colour looks washed out:

```toml
# ~/.config/koda/config.toml
[tui]
syntax_highlight = false
```

It covers `Read` output, `Bash` headers and inline code, and is read when the
TUI starts; restart Koda after changing it.

## Limiting what a turn spends

`turn_token_budget` caps the tokens one turn spends: the model calls of the
turn itself, its sub-agents' and any compaction's. For a scheduled or
unattended run, set it in `config.toml`, or for one run with
`koda exec -c turn_token_budget=200000 "…"`.

- **What counts** is Codex's rule for a goal's budget: input tokens not read
  from the provider's cache, plus output tokens. A long session isn't charged
  again for the history its cache serves.
- **Pacing.** A Claude model that takes the API's
  [task budget](https://platform.claude.com/docs/en/build-with-claude/task-budgets)
  (Haiku 5.5, Sonnet 5.5, Opus 5.5 and others) is sent the budget and sees it
  count down. Any other model is told once, at 90% of the budget, to wrap up:
  start nothing new, summarize what's done and what's left.
- **The stop**, as Claude Code's `--max-budget-usd`: Koda checks the budget
  as each model call returns. The call that reaches it completes and its
  tools run; then the turn makes no further call, a running sub-agent stops at
  its next call, and a new one is refused. Spend can pass the budget by that
  one call, so leave headroom.
- **How it ends:** what the turn did stays in the session, and a follow-up
  continues with a fresh budget. The turn fails with
  `Turn failed: token budget spent (…)`; over the
  [protocol](./headless.md#the-client-protocol) its `error.code` is
  `tokenBudgetSpent`, and `koda exec` exits 1.

The budget must be at least 20,000 tokens, the Claude API's smallest task
budget; a smaller one stops Koda at startup.

