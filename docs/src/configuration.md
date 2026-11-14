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

Settings don't come from environment variables, as Codex's don't. The ones
earlier versions read (`KODA_MODEL`, `KODA_MODE`, `KODA_READ_TIMEOUT_SECS`
and others) are ignored, with a warning at startup that names each one's
replacement.

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

`~/.config/koda/config.toml` holds the settings a new session starts with,
as Codex keeps `~/.codex/config.toml`. `/model`, `/provider` and `koda mcp`
write it, and you can edit it by hand:

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
| `model_post_turn_compact_threshold_percent` | Compact after a turn's final reply once this share of the window is used (1–100; 0 turns it off) |
| `mode` | The mode a new session starts in: `auto`, `safe`, or `plan` (`koda exec` only) |
| `[model_providers.<id>]` | Per provider: `base_url`, `request_max_retries`, `stream_max_retries`, `stream_idle_timeout_ms` (0 turns the limit off) |
| `[mcp_servers.<name>]` | An MCP server every new session starts |
| `[tui] syntax_highlight` | `false` shows code and diffs without colour. Read by the TUI from its own machine's file |
| `[sandbox] allow_local_binding` | `true` lets sandboxed commands on macOS use any local port (see [Sandbox](./sandbox.md#when-a-command-needs-to-leave-the-sandbox)) |

**`-c key=value` overrides a key for one run,** as Codex's `-c` does, and is
never saved: `koda -c model=claude-opus`, or
`koda exec -c model_providers.lm-studio.base_url=http://127.0.0.1:1234/v1 "…"`.
The value is read as TOML (`5`, `true`, `"text"`), and as plain text when it
isn't. A key Koda doesn't know stops the run. Flags such as `--model` win over
`-c`. A setting the agent's JSON gives wins over the file and `-c`, for that
agent.

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
1. --max-context-tokens N
2. "max_context_tokens" in the agent JSON
3. model_context_window in config.toml (or -c model_context_window=N)
4. What the server reports (LM Studio, llama-server, Ollama, Anthropic, Gemini)
5. Koda's built-in model table
```

Use 1 or 3 when the server loaded a different size than Koda assumed, or can't
say (an OpenAI-compatible engine such as `rec` falls to the table, which
assumes 128,000 for most local models). A pinned value also stays when you
switch models with `/model`. It must be a positive whole number; `0` or
text stops Koda at startup with an error rather than being ignored. Startup
logs `Context window from override: N tokens for <model>`.

```bash
koda exec --max-context-tokens 65536 "say hi" --provider lm-studio --base-url http://127.0.0.1:11500/v1
```

## Config files

Everything lives in `~/.config/koda/`:

| Path | Content |
|------|---------|
| `config.toml` | Your settings: the model choice, provider endpoints, MCP servers (see [The config file](#the-config-file)) |
| `db/koda.db` | SQLite — sessions, messages, API keys |
| `history.jsonl` | What you typed in the TUI, for ↑ / ↓: the newest 500 entries, one JSON object per line, readable by you only |
| `logs/koda-<pid>.log` | Tracing log, one per process; `logs/latest` links to the newest (see [Debugging a run](./debugging.md)) |
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

> **Removed in v0.2.26**: `KODA_TRANSCRIPT_HYPERLINKS` and
> `KODA_EXPORT_VERBOSE` previously controlled `/export` output
> formatting. Both were removed alongside `/export` itself when RFC
> #1167 collapsed transcript export into [`/debug-bundle`](commands.md#debug-bundle).
> Setting either var is now a silent no-op; remove them from your
> shell rc files.
