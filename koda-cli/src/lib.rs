//! # Koda CLI
//!
//! User-facing interfaces for the [Koda](https://github.com/lijunzh/koda)
//! personal AI assistant. The engine lives in [`koda_core`] — this crate
//! handles presentation.
//!
//! ## Entry points
//!
//! | Mode | Invocation | Module |
//! |------|-----------|--------|
//! | **TUI** (default) | `koda` | [`tui_app`] |
//! | **Headless** | `koda "prompt"` or `koda -p "..."` | [`headless`] |
//! | **ACP server** | `koda server --stdio` | [`server`], [`acp_adapter`] |
//!
//! ## CLI reference
//!
//! ### Flags
//!
//! | Flag | Env var | Description |
//! |------|---------|-------------|
//! | `-p`, `--prompt <PROMPT>` | | Run a single prompt and exit (headless). Use `"-"` for stdin |
//! | `<PROMPT>` (positional) | | Same as `-p` — `koda "fix the bug"` works |
//! | `-a`, `--agent <NAME>` | | Agent to use (matches JSON in `agents/`, default: `default`) |
//! | `-s`, `--resume <ID>` | | Resume a previous session by ID |
//! | `--model <NAME>` | `KODA_MODEL` | Model name or alias (e.g. `claude-sonnet`, `gemini-flash`) |
//! | `--provider <NAME>` | `KODA_PROVIDER` | LLM provider (`anthropic`, `gemini`, `openai`, `ollama`, …) |
//! | `--base-url <URL>` | `KODA_BASE_URL` | Override the provider's API base URL |
//! | `--max-tokens <N>` | | Maximum output tokens |
//! | `--temperature <F>` | | Sampling temperature (0.0–2.0) |
//! | `--thinking-budget <N>` | | Anthropic extended thinking budget (tokens) |
//! | `--reasoning-effort <L>` | | OpenAI reasoning effort (`low`, `medium`, `high`) |
//! | `--output-format <FMT>` | | Headless output format: `text` (default) or `json` |
//! | `--project-root <DIR>` | | Project root (defaults to cwd) |
//!
//! ### Subcommands
//!
//! | Command | Description |
//! |---------|-------------|
//! | `koda server --stdio` | Start ACP server over stdin/stdout (for editors) |
//! | `koda server --port <N>` | Start ACP server on TCP port (default: 9999) |
//!
//! ## Quick start
//!
//! ```bash
//! # Interactive REPL (auto-detects local models)
//! koda
//!
//! # One-shot with positional prompt
//! koda "fix the failing test"
//!
//! # Explicit model alias
//! koda -p "explain auth.rs" -m opus
//!
//! # Piped input
//! echo "review this diff" | koda
//!
//! # ACP server for editor integration
//! koda server --stdio
//! ```
//!
//! ## Slash commands
//!
//! Type these in the REPL input. Tab-completion is supported.
//!
//! | Command | Description |
//! |---------|-------------|
//! | `/help` | Show available commands and keybindings |
//! | `/model <name>` | Switch model — aliases like `opus`, `sonnet`, `flash` |
//! | `/provider` | Browse all models from a provider |
//! | `/compact` | Summarize old context to free tokens |
//! | `/diff` | Show uncommitted changes (review or commit) |
//! | `/undo` | Revert last turn's file mutations |
//! | `/sessions` | List, resume, or delete past sessions |
//! | `/memory` | View/edit project and global memory files |
//! | `/skills` | List available skills (search with query) |
//! | `/agent <name>` | Switch to a sub-agent |
//! | `/key` | Manage API keys |
//! | `/expand` | Replay last tool output (full, untruncated) |
//! | `/verbose` | Toggle full tool output |
//! | `/purge <days>` | Delete archived history older than N days |
//! | `/exit` | Quit the session |
//!
//! ## Keybindings
//!
//! ### Input
//!
//! | Key | Action |
//! |-----|--------|
//! | `Enter` | Send message |
//! | `Alt+Enter` | Insert newline (multi-line input) |
//! | `Tab` | Autocomplete slash commands and `@file` paths |
//! | `Shift+Tab` | Toggle approval mode (auto ↔ confirm) |
//! | `↑ / ↓` | Cycle through input history |
//! | `Ctrl+R` | Reverse history search |
//!
//! ### Navigation
//!
//! | Key | Action |
//! |-----|--------|
//! | `PgUp / PgDn` | Scroll history one page up / down |
//! | `Home` | Jump to top of history |
//! | `End` | Jump to bottom (latest output) |
//! | Mouse scroll | Scroll conversation history |
//! | `Ctrl+Y` | Copy last code block to clipboard |
//! | `Ctrl+U` | Copy last assistant response to clipboard |
//!
//! ### Session control
//!
//! | Key | Action |
//! |-----|--------|
//! | `Esc` | Cancel current inference |
//! | `Ctrl+C` | Cancel current inference |
//! | `Ctrl+D` | Quit koda |
//!
//! ### Approval prompt
//!
//! These keys appear when the agent asks to execute a tool:
//!
//! | Key | Action |
//! |-----|--------|
//! | `y` | Approve this action |
//! | `n` | Reject this action |
//! | `a` | Approve and switch to auto mode |
//! | `f` | Reject with typed feedback |
//! | `Esc` | Reject |
//!
//! ## Approval modes
//!
//! Koda has two approval modes, toggled with `Shift+Tab`:
//!
//! - **Auto** — approve all non-destructive actions automatically.
//!   Destructive commands (`rm`, `sudo`, `git push --force`, etc.) still
//!   require confirmation.
//! - **Confirm** — every write/mutation requires explicit `y` before
//!   executing. Read-only tools (Read, Grep, Glob) are always auto-approved.
//!
//! In headless mode (`koda "prompt"`), destructive actions are **rejected**
//! outright — there's no human to approve them.
//!
//! ## Memory
//!
//! Koda reads memory files that persist across sessions:
//!
//! - **Project memory** — `MEMORY.md` (or `CLAUDE.md`, `AGENTS.md`) in the
//!   project root. Injected into every system prompt for this project.
//! - **Global memory** — `~/.config/koda/memory.md`. Injected into every
//!   system prompt across all projects.
//!
//! Use `/memory` to view and edit, or the `MemoryWrite` tool to append facts
//! during a conversation.
//!
//! ## Custom agents
//!
//! Place JSON files in `.koda/agents/` (project) or `~/.config/koda/agents/`
//! (global):
//!
//! ```json
//! {
//!   "name": "testgen",
//!   "system_prompt": "You are a test generation specialist.",
//!   "model": "gemini-2.5-flash",
//!   "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
//! }
//! ```
//!
//! The model dispatches to sub-agents via the `InvokeAgent` tool. Each agent
//! runs in its own session with its own model, tools, and system prompt.
//!
//! ## Skills
//!
//! Skills are reusable expertise modules (markdown files with structured
//! instructions). Built-in skills include code review and security audit.
//!
//! - List skills: `/skills` or the `ListSkills` tool
//! - Activate a skill: `/skills <query>` or the `ActivateSkill` tool
//! - Create custom skills: place `.md` files in `.koda/skills/` or
//!   `~/.config/koda/skills/`
//!
//! ## Configuration precedence
//!
//! When multiple sources specify the model, provider, or API key, the
//! **highest-priority source wins**:
//!
//! ```text
//! 1. CLI flags          --model, --provider, --base-url
//!        ↓ (override)
//! 2. Env vars           KODA_MODEL, KODA_PROVIDER, KODA_BASE_URL
//!        ↓ (set if not already in env)
//! 3. Keystore / DB      saved by /model, /provider, /key (injected at startup)
//!        ↓ (fallback)
//! 4. Built-in defaults  Claude Sonnet via Anthropic
//! ```
//!
//! API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …)
//! follow the same chain. Keys saved with `/key` are stored in the local
//! SQLite keystore and injected into the process environment at startup —
//! but a key already in the shell environment takes precedence.
//!
//! **CI / scripting:** the interactive `/key`, `/model`, `/provider`
//! wizards are great for local setup, but in automation always prefer
//! env vars or CLI flags — they work without a terminal and compose
//! cleanly with `direnv`, Docker, and GitHub Actions secrets.
//!
//! ```bash
//! # Override model for one call without touching saved config
//! koda "review auth.rs" --model o3
//!
//! # Per-project model via direnv (.envrc)
//! export KODA_MODEL=gemini-2.5-pro
//!
//! # CI pipeline
//! ANTHROPIC_API_KEY=${{ secrets.ANTHROPIC_KEY }} koda -p "check types"
//! ```
//!
//! ## Providers and model aliases
//!
//! Koda supports 14 LLM providers. Model aliases route across providers
//! without remembering full model IDs:
//!
//! | Alias | Provider | Model |
//! |-------|----------|-------|
//! | `gemini-flash-lite` | Gemini | gemini-flash-lite-latest |
//! | `gemini-flash` | Gemini | gemini-flash-latest |
//! | `gemini-pro` | Gemini | gemini-pro-latest |
//! | `claude-haiku` | Anthropic | claude-haiku-4-5 |
//! | `claude-sonnet` | Anthropic | claude-sonnet-4-6 |
//! | `claude-opus` | Anthropic | claude-opus-4-6 |
//!
//! Local providers (LM Studio, Ollama, vLLM) are auto-detected on first run
//! and require no API key.
//!
//! ## Configuration
//!
//! Everything lives in `~/.config/koda/`:
//!
//! | Path | Content |
//! |------|---------|
//! | `db/koda.db` | SQLite — sessions, messages, settings, API keys, history |
//! | `logs/` | Tracing logs (human-readable) |
//! | `agents/` | Global custom agent JSON files |
//! | `skills/` | Global custom skill markdown files |
//! | `memory.md` | Global memory (injected into all system prompts) |
//!
//! ## Privacy and data
//!
//! Koda has **zero telemetry**. No usage data, crash reports, or analytics
//! are collected or transmitted. All data stays local:
//!
//! - Conversations are stored in your local SQLite database
//! - API keys are stored locally in the same database (file mode 0600)
//! - The only network traffic is your LLM API calls to the provider you choose
//! - No phone-home, no update checks to third-party servers (version checks
//!   query crates.io only)

pub mod acp_adapter;
pub mod ansi_parse;
pub mod completer;
pub mod diff_render;
pub mod headless;
pub mod highlight;
pub mod history_render;
pub mod input;
pub mod md_render;
pub mod mouse_select;
pub mod onboarding;
pub mod repl;
pub mod scroll_buffer;
pub mod server;
pub mod sink;
pub mod startup;
pub mod tool_history;
pub mod tui_app;
pub mod tui_commands;
pub mod tui_context;
pub mod tui_handlers_inference;
pub mod tui_output;
pub mod tui_render;
pub mod tui_types;
pub mod tui_viewport;
pub mod tui_wizards;
pub mod widgets;
pub mod wrap_input;
pub mod wrap_util;
