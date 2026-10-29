# Configuration

## Precedence

When multiple sources specify the model, provider, or API key, the
**highest-priority source wins**:

```text
1. CLI flags          --model, --provider, --base-url        (highest)
       ↓
2. Shell env vars     KODA_MODEL, KODA_PROVIDER, KODA_BASE_URL
       ↓
3. Keystore / DB      saved by /model, /provider, /key (injected at startup)
       ↓
4. Built-in defaults  Claude Sonnet via Anthropic              (lowest)
```

API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …)
follow the same chain. Keys saved with `/key` are injected at startup —
but a key already in the shell environment takes precedence and is never
overwritten.

```bash
# Per-call override (doesn't change saved config)
koda "review auth.rs" --model o3

# Per-project via direnv (.envrc)
export KODA_MODEL=gemini-2.5-pro

# CI / GitHub Actions
ANTHROPIC_API_KEY=${{ secrets.ANTHROPIC_KEY }} koda -p "check types"
```

## Context window

Koda needs the model's context window to show the footer percentage, warn
before a request overflows, and auto-compact. It takes the first of these:

```text
1. --max-context-tokens N, then KODA_MAX_CONTEXT_TOKENS=N
2. "max_context_tokens" in the agent JSON
3. What the server reports (LM Studio, llama-server, Ollama, Anthropic, Gemini)
4. Koda's built-in model table
```

Use 1 when the server loaded a different size than Koda assumed, or can't
say (an OpenAI-compatible engine such as `rec` falls to the table, which
assumes 128,000 for most local models). A pinned value also stays when you
switch models with `/model`. It must be a positive whole number; `0` or
text stops Koda at startup with an error rather than being ignored. Startup
logs `Context window from override: N tokens for <model>`.

```bash
KODA_MAX_CONTEXT_TOKENS=65536 koda -p "say hi" --provider lm-studio --base-url http://127.0.0.1:11500/v1
```

## Config files

Everything lives in `~/.config/koda/`:

| Path | Content |
|------|---------|
| `db/koda.db` | SQLite — sessions, messages, settings, API keys, input history |
| `logs/koda.log` | Rolling daily tracing log (not shown in the TUI) |
| `agents/` | Global custom agent JSON definitions |
| `skills/` | Global custom skill markdown files |
| `memory.md` | Global memory (injected into all system prompts) |

Project-level overrides live in `.koda/` at your project root and take
priority over global config:

| Path | Content |
|------|---------|
| `.koda/agents/` | Project-specific agent definitions |
| `.koda/skills/` | Project-specific skills |
| `MEMORY.md` | Project memory (also checks `CLAUDE.md`, `AGENTS.md`) |

## Display

Koda's TUI ships ANSI-colored syntax highlighting by default. Disable
with one flag for environments where the output looks awful (monochrome
themes, non-RGB-aware terminals).

| Env var | Default | Disable with |
|---------|---------|--------------|
| `KODA_SYNTAX_HIGHLIGHT` | on | `off`, `0`, `false`, or `no` |

- **`KODA_SYNTAX_HIGHLIGHT`** controls TUI syntax highlighting for
  `Read`, `Bash` headers, and inline code. Disable for monochrome
  terminals or terminals where ANSI-RGB output looks washed out.
  Memoized on first call — restart koda after changing.

The negative values above (`off`, `0`, `false`, `no`) are
case-insensitive; everything else (including empty string and `on`)
keeps the feature enabled.

> **Removed in v0.2.26**: `KODA_TRANSCRIPT_HYPERLINKS` and
> `KODA_EXPORT_VERBOSE` previously controlled `/export` output
> formatting. Both were removed alongside `/export` itself when RFC
> #1167 collapsed transcript export into [`/debug-bundle`](commands.md#debug-bundle).
> Setting either var is now a silent no-op; remove them from your
> shell rc files.
