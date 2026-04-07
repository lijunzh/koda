//! # Koda — User Manual
//!
//! Koda is a terminal-native AI coding agent. It runs locally, keeps all
//! data on your machine, and connects to any LLM provider you choose.
//!
//! This page is the **complete reference**. Use Ctrl+F to jump to any topic.
//!
//! ## Modes at a glance
//!
//! | Mode | How to invoke | Best for |
//! |------|--------------|----------|
//! | **Interactive TUI** | `koda` (no args) | Long sessions, iterative coding |
//! | **Headless** | `koda "prompt"` or `echo … \| koda` | Scripts, CI, one-shot tasks |
//! | **ACP server** | `koda server --stdio` | Editor plugins (VS Code, Zed, …) |
//!
//! ---
//!
//! ## Quick start
//!
//! ```bash
//! # 1. Open the interactive TUI
//! koda
//!
//! # 2. Ask something at the prompt
//! #    > explain why the auth tests are failing
//!
//! # 3. Type /help inside for keybindings and commands
//! ```
//!
//! First run triggers onboarding: Koda looks for a running local model
//! (LM Studio, Ollama) and falls back to prompting for a cloud API key.
//!
//! ---
//!
//! ## CLI reference
//!
//! ### Flags
//!
//! | Flag | Env var | Description |
//! |------|---------|-------------|
//! | `-p`, `--prompt <PROMPT>` | | Run a single prompt and exit (headless). Use `"-"` for stdin |
//! | `<PROMPT>` (positional) | | Same as `-p` — `koda "fix the bug"` works |
//! | `-a`, `--agent <NAME>` | | Agent definition to use (JSON in `agents/`, default: `default`) |
//! | `-s`, `--resume <ID>` | | Resume a previous session by ID prefix |
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
//! | `koda server --port <N>` | WebSocket ACP server on port N (not yet implemented) |
//!
//! ---
//!
//! ## Headless mode
//!
//! Headless mode runs a single prompt, prints the answer, and exits.
//! No TUI — the assistant's reply streams to stdout; tool status goes to stderr.
//!
//! ```bash
//! # Positional prompt (shortest form)
//! koda "what does this codebase do?"
//!
//! # Explicit -p flag
//! koda -p "fix the failing tests"
//!
//! # Read prompt from stdin (use "-" literally)
//! koda -p - < my_question.txt
//!
//! # Pipe into koda — stdin is auto-detected when not a TTY
//! git diff HEAD~1 | koda
//! cat error.log | koda
//! echo "review auth.rs" | koda
//!
//! # With a model override
//! koda "explain this" --model gemini-flash
//!
//! # Capture just the assistant reply (tool status stays on stderr)
//! koda "list exported functions in lib.rs" > functions.txt
//! ```
//!
//! ### Exit codes
//!
//! | Code | Meaning |
//! |------|---------|
//! | `0` | Turn completed successfully |
//! | `1` | Error (API failure, bad config, …) |
//!
//! ### Approval in headless mode
//!
//! There is no human to approve tool calls. Koda automatically:
//!
//! - **Approves** read-only tools: Read, Grep, Glob, WebFetch
//! - **Approves** safe write tools: Write, Edit (files only)
//! - **Rejects** destructive Bash commands (`rm -rf`, `git push --force`, …)
//! - **Skips** AskUser questions (prints to stderr, continues with empty answer)
//!
//! Rejected actions are printed to stderr. The turn still completes with
//! exit code 0 unless the API itself errors.
//!
//! ### Output formats
//!
//! `--output-format text` (default) — streams the assistant's reply to stdout
//! exactly as typed. Tool call summaries go to stderr.
//!
//! `--output-format json` — emits a single JSON object after the turn ends:
//!
//! ```json
//! {
//!   "success": true,
//!   "response": "The exported functions are …",
//!   "session_id": "a3f8bc12-…",
//!   "model": "claude-sonnet-4-6"
//! }
//! ```
//!
//! ### File attachment in headless mode
//!
//! The `@file` syntax works in headless mode too:
//!
//! ```bash
//! koda "review @src/auth.rs and @tests/auth_test.rs"
//! ```
//!
//! ### Resuming a session in headless mode
//!
//! ```bash
//! # Note the session ID shown in the TUI status bar, then continue from a script
//! koda -s a3f8bc "run the failing tests and fix them"
//! ```
//!
//! ---
//!
//! ## Interactive mode (TUI)
//!
//! Run `koda` with no arguments to open the full-screen TUI.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │  [conversation history — scrollable with PgUp/PgDn]                     │
//! │                                                                          │
//! │  ⚡ Bash   cargo test                                                    │
//! │  │ running 42 tests …                                                   │
//! │  ✓ Bash (exit 0)                                                         │
//! │                                                                          │
//! │  All tests pass! Here's what I changed in `auth.rs` …                   │
//! ├────────────── claude-sonnet · auto · 34% · 8s ───────────────────────────┤
//! │  > _                                                                     │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The status bar shows: **model** · **approval mode** · **context %** · **elapsed**
//!
//! ---
//!
//! ## File and image attachment
//!
//! Type `@` anywhere in your message to attach files as context:
//!
//! ```text
//! > explain @src/auth.rs
//! > compare @old_impl.rs and @new_impl.rs
//! > what's wrong with @error.log
//! ```
//!
//! As you type after `@`, a fuzzy file picker appears. Press `Tab` to cycle
//! through matches or select with `Enter`. The file's full contents are
//! injected into the message before it's sent to the model.
//!
//! ### Images
//!
//! For vision-capable models (Claude, Gemini, GPT-4o), attach images directly:
//!
//! ```text
//! > what does @screenshot.png show?
//! > explain the architecture in @diagram.svg
//! ```
//!
//! Supported formats: PNG, JPEG, GIF, WEBP. Images are base64-encoded and
//! sent inline to the model API.
//!
//! ### Large pastes
//!
//! Pasting more than ~500 characters into the input is automatically wrapped
//! in a reference block to keep prompts clean:
//!
//! ```text
//! > [pasted 1,234 chars — attached as reference]
//! > what's the bug in this code?
//! ```
//!
//! The paste is still sent to the model; it just doesn't clutter the display.
//!
//! ---
//!
//! ## Slash commands
//!
//! Type in the TUI input. Tab-completion is available for all commands.
//!
//! ### `/help`
//!
//! Shows the quick-reference keybinding card inside the TUI.
//! This docs page is the full reference; `/help` is the in-session reminder.
//!
//! ### `/model [<alias-or-id>]`
//!
//! Without an argument: opens an interactive picker listing all model aliases
//! and any locally running models detected via LM Studio or Ollama.
//!
//! With an argument: switches immediately.
//!
//! ```text
//! /model gemini-flash        ← switch by alias
//! /model claude-opus         ← switch by alias
//! /model local               ← auto-detect from LM Studio
//! /model gpt-4o              ← literal model ID (no alias needed)
//! /model llama3.2            ← any model name your provider understands
//! ```
//!
//! The new model is persisted to the keystore and used for all future
//! sessions until changed again. See [**Providers and model aliases**](#providers-and-model-aliases).
//!
//! ### `/provider [<name>]`
//!
//! Without an argument: opens a two-step picker — choose provider, then
//! browse and pick one of its available models.
//!
//! With an argument: jumps straight to that provider's model list.
//!
//! ```text
//! /provider                  ← open the picker
//! /provider anthropic        ← go straight to Anthropic models
//! /provider ollama           ← browse locally running Ollama models
//! ```
//!
//! ### `/key`
//!
//! Opens the API key manager. Select a provider, then type or paste your key.
//! Keys are stored in the local SQLite keystore (file mode 0600) and injected
//! as environment variables at every startup.
//!
//! Shell env vars always win over stored keys — so `export ANTHROPIC_API_KEY=…`
//! in your shell or `.envrc` is always a clean override.
//!
//! ### `/compact`
//!
//! Summarises old conversation history to free context tokens. Koda
//! auto-compacts when the context window hits **85%** full, but you can
//! trigger it manually at any time:
//!
//! - All but the **last 4 messages** are summarised by the model
//! - The summary replaces the old messages in the DB
//! - The compressed session continues normally
//! - Use `/purge` later to clean up the archived messages
//!
//! ### `/purge [<age>]`
//!
//! Deletes compacted (archived) message history. Does not touch the live messages
//! in your current session.
//!
//! ```text
//! /purge        ← delete all archived messages (prompts for confirmation)
//! /purge 90d    ← only messages archived more than 90 days ago
//! /purge 30d    ← only messages archived more than 30 days ago
//! ```
//!
//! Requires `y` to confirm. Deleted messages are gone permanently.
//!
//! ### `/undo`
//!
//! Reverts all file mutations from the **previous inference turn** — Write,
//! Edit, and Delete tool calls. One `/undo` per turn; call again to go back
//! another turn. Bash commands (e.g. `cargo build`) are **not** undoable.
//!
//! ```text
//! # Koda wrote bad code in the last turn
//! /undo    ← all file changes from that turn are reverted
//! /undo    ← undo the turn before that
//! ```
//!
//! ### `/diff`
//!
//! Shows a summary of uncommitted `git diff` in the project root. Then offers:
//!
//! - **Review** — sends the diff to the model for code review comments
//! - **Commit** — asks the model to write a conventional commit message and
//!   runs `git commit -m "…"`
//!
//! ### `/sessions [<sub-command>]`
//!
//! ```text
//! /sessions              ← open the session picker (shows last 100 sessions)
//! /sessions resume abc   ← resume the session whose ID starts with "abc"
//! /sessions delete abc   ← permanently delete that session
//! ```
//!
//! Session IDs are UUIDs; you only need 6–8 characters to be unambiguous.
//! On resume, Koda shows an away-summary: idle time, message count, token
//! usage, and a banner if the previous turn was interrupted mid-inference.
//!
//! ### `/memory [save]`
//!
//! ```text
//! /memory        ← show the paths to project and global memory files
//! /memory save   ← ask the model to summarise the session and append to MEMORY.md
//! ```
//!
//! See [**Memory**](#memory) for the full memory system.
//!
//! ### `/skills [<query>]`
//!
//! ```text
//! /skills              ← list all built-in and custom skills
//! /skills security     ← filter by name or description
//! ```
//!
//! ### `/agent <name>`
//!
//! Switches to a named sub-agent for the current session. The agent's
//! system prompt, model, and allowed tools replace the current defaults.
//!
//! ```text
//! /agent testgen     ← use the "testgen" agent definition
//! ```
//!
//! ### `/expand [<n>]`
//!
//! Shows the full, untruncated output of a recent tool call. Useful when Koda
//! collapsed a long `cargo build` or `grep` result during streaming.
//!
//! ```text
//! /expand      ← show full output of the most recent tool call
//! /expand 3    ← show full output of the 3rd most recent tool call
//! ```
//!
//! ### `/verbose [on|off]`
//!
//! Toggles verbose tool output. By default Koda collapses long outputs
//! during streaming. Verbose mode shows every line in real time.
//!
//! ```text
//! /verbose      ← toggle
//! /verbose on   ← enable explicitly
//! /verbose off  ← disable explicitly
//! ```
//!
//! ### `/exit`
//!
//! Quit Koda. Equivalent to `Ctrl+D`.
//!
//! ---
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
//! | `a` | Approve and switch to auto mode (no more confirmations this session) |
//! | `f` | Reject and type written feedback explaining why |
//! | `Esc` | Reject (same as `n`) |
//!
//! ---
//!
//! ## Approval modes
//!
//! Koda has two approval modes, toggled with `Shift+Tab` (current mode shown
//! in the status bar):
//!
//! **Auto** — safe tools run without confirmation. Destructive shell commands
//! (`rm -rf`, `sudo`, `git push --force`, etc.) still require explicit `y`.
//! Read-only tools (Read, Grep, Glob, WebFetch) are always auto-approved.
//!
//! **Confirm** — every write or mutation requires explicit `y` before executing.
//! Read-only tools are still auto-approved.
//!
//! The mode is **persisted per session** — if you approve with `a` (auto),
//! that session remembers it even after resuming.
//!
//! In headless mode, there is no human to prompt. Destructive Bash commands
//! are silently rejected; all other tools proceed automatically.
//!
//! ---
//!
//! ## Session management
//!
//! Koda stores every conversation in a local SQLite database, organised by
//! project root. Each session gets a UUID that you can use to resume it.
//!
//! ```bash
//! # List and pick a session interactively
//! /sessions
//!
//! # Resume by ID prefix from the TUI
//! /sessions resume a3f8bc
//!
//! # Resume from the command line (headless or interactive)
//! koda -s a3f8bc
//! koda -s a3f8bc "continue where we left off"
//!
//! # Delete a session permanently
//! /sessions delete a3f8bc
//! ```
//!
//! **Away summary** — when you resume a session that was idle, Koda shows:
//! - How long you were away
//! - Message and tool-call counts
//! - Total tokens used
//! - A banner if the previous turn was interrupted mid-inference
//!
//! **Session title** — Koda auto-generates a short title after the first
//! exchange. The title is shown in `/sessions` and the status bar.
//!
//! ---
//!
//! ## Context management
//!
//! Every provider has a context window limit (measured in tokens). The status
//! bar shows current usage as a percentage (e.g. `34%`).
//!
//! ### Auto-compact
//!
//! When usage reaches **85%**, Koda automatically compacts the session:
//!
//! 1. All but the last 4 messages are summarised by the model
//! 2. The summary is stored in the DB (recoverable with `/purge`)
//! 3. A status line appears: `🐻 Context at 85% — auto-compacting…`
//!
//! Auto-compact is skipped if there are pending tool calls (it waits for the
//! turn to finish cleanly).
//!
//! ### Manual compact
//!
//! Run `/compact` at any time to compact early, e.g. before starting a large
//! refactor so you have the full context window available.
//!
//! ### Purging archived history
//!
//! `/compact` keeps summaries in the DB. Use `/purge` to delete them:
//!
//! ```text
//! /purge        ← prompt and delete all archived messages
//! /purge 90d    ← delete only archived messages older than 90 days
//! ```
//!
//! ---
//!
//! ## Memory
//!
//! Memory files persist facts and preferences across sessions.
//!
//! **Project memory** — `MEMORY.md` in the project root (Koda also reads
//! `CLAUDE.md` and `AGENTS.md` for compatibility). Injected into every
//! system prompt for that project.
//!
//! **Global memory** — `~/.config/koda/memory.md`. Injected into every
//! system prompt across all projects.
//!
//! ```text
//! /memory        ← show the paths to both memory files
//! /memory save   ← ask the model to summarise and append to MEMORY.md
//! ```
//!
//! The `MemoryWrite` tool lets the model append facts to memory directly
//! during a conversation:
//!
//! ```text
//! > Remember that we use tabs not spaces in this project
//! ```
//! (Koda will call `MemoryWrite` automatically when you ask it to remember.)
//!
//! ---
//!
//! ## Providers and model aliases
//!
//! ### All supported providers
//!
//! | Provider name | `--provider` value | API key env var | Default model | Needs key |
//! |---|---|---|---|---|
//! | Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | claude-sonnet-4-6 | ✓ |
//! | OpenAI | `openai` | `OPENAI_API_KEY` | gpt-4o | ✓ |
//! | Google Gemini | `gemini` | `GEMINI_API_KEY` | gemini-flash-latest | ✓ |
//! | Groq | `groq` | `GROQ_API_KEY` | llama-3.3-70b-versatile | ✓ |
//! | Grok / xAI | `grok` | `XAI_API_KEY` | grok-3 | ✓ |
//! | DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | deepseek-chat | ✓ |
//! | Mistral | `mistral` | `MISTRAL_API_KEY` | mistral-large-latest | ✓ |
//! | MiniMax | `minimax` | `MINIMAX_API_KEY` | minimax-text-01 | ✓ |
//! | OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | anthropic/claude-3.5-sonnet | ✓ |
//! | Together AI | `together` | `TOGETHER_API_KEY` | Llama-3.3-70B-Instruct-Turbo | ✓ |
//! | Fireworks AI | `fireworks` | `FIREWORKS_API_KEY` | llama-v3p3-70b-instruct | ✓ |
//! | LM Studio | `lm-studio` | — | auto-detect | ✗ |
//! | Ollama | `ollama` | — | auto-detect | ✗ |
//! | vLLM | `vllm` | — | auto-detect | ✗ |
//!
//! Local providers (LM Studio, Ollama, vLLM) are auto-detected on first run
//! and require no API key. The model is discovered from the running server.
//!
//! ### Model aliases
//!
//! Aliases let you switch models without memorising exact IDs. They're shown
//! in the `/model` picker and accepted by `--model` and `/model`.
//!
//! | Alias | Provider | Exact model ID |
//! |-------|----------|----------------|
//! | `gemini-flash-lite` | Gemini | `gemini-flash-lite-latest` |
//! | `gemini-flash` | Gemini | `gemini-flash-latest` |
//! | `gemini-pro` | Gemini | `gemini-pro-latest` |
//! | `claude-haiku` | Anthropic | `claude-haiku-4-5-20251001` |
//! | `claude-sonnet` | Anthropic | `claude-sonnet-4-6` |
//! | `claude-opus` | Anthropic | `claude-opus-4-6` |
//! | `local` | LM Studio | auto-detect at runtime |
//!
//! You can also use **any literal model ID** your provider supports — aliases
//! are just shortcuts. `koda --model gpt-4o-mini` or `/model o3` both work.
//!
//! ---
//!
//! ## Configuration precedence
//!
//! When multiple sources specify the model, provider, or API key, the
//! **highest-priority source wins**:
//!
//! ```text
//! 1. CLI flags          --model, --provider, --base-url        (highest)
//!        ↓
//! 2. Shell env vars     KODA_MODEL, KODA_PROVIDER, KODA_BASE_URL
//!        ↓
//! 3. Keystore / DB      saved by /model, /provider, /key (injected at startup)
//!        ↓
//! 4. Built-in defaults  Claude Sonnet via Anthropic              (lowest)
//! ```
//!
//! API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …)
//! follow the same chain. Keys saved with `/key` are injected at startup —
//! but a key already in the shell environment takes precedence and is never
//! overwritten.
//!
//! ```bash
//! # Per-call override (doesn't change saved config)
//! koda "review auth.rs" --model o3
//!
//! # Per-project via direnv (.envrc)
//! export KODA_MODEL=gemini-2.5-pro
//!
//! # CI / GitHub Actions
//! ANTHROPIC_API_KEY=${{ secrets.ANTHROPIC_KEY }} koda -p "check types"
//! ```
//!
//! ---
//!
//! ## Custom agents
//!
//! Place JSON files in `.koda/agents/` (project-local) or
//! `~/.config/koda/agents/` (global):
//!
//! ```json
//! {
//!   "name": "testgen",
//!   "system_prompt": "You are a test generation specialist. When asked to write tests, always use the project's existing test patterns.",
//!   "model": "gemini-2.5-flash",
//!   "allowed_tools": ["Read", "Write", "Edit", "Bash", "Grep", "Glob"]
//! }
//! ```
//!
//! | Field | Required | Description |
//! |-------|----------|-------------|
//! | `name` | ✓ | Identifier used with `/agent <name>` and `InvokeAgent` |
//! | `system_prompt` | ✓ | The agent's persona and instructions |
//! | `model` | | Model alias or ID (defaults to current saved model) |
//! | `allowed_tools` | | Subset of tools the agent can call (defaults to all) |
//!
//! The main model dispatches to sub-agents via the `InvokeAgent` tool. Each
//! sub-agent runs in its own worktree with its own model, tools, and session.
//!
//! ---
//!
//! ## Skills
//!
//! Skills are reusable expertise modules — markdown files loaded into the
//! system prompt on demand. Built-in skills include code review and security
//! audit.
//!
//! ```text
//! /skills                  ← list all available skills
//! /skills security         ← filter by name or description
//! ```
//!
//! The model can also activate skills automatically via the `ActivateSkill`
//! tool when it determines a skill is relevant.
//!
//! ### Creating custom skills
//!
//! Place `.md` files in `.koda/skills/` (project-local) or
//! `~/.config/koda/skills/` (global). The filename becomes the skill name.
//!
//! ```markdown
//! # My Review Checklist
//!
//! When reviewing code, always check:
//! - [ ] No hardcoded secrets
//! - [ ] Error handling covers all paths
//! - [ ] Tests cover the new logic
//! ```
//!
//! ---
//!
//! ## Tools reference
//!
//! Koda exposes these tools to the model. In **Confirm** approval mode you'll
//! be prompted before each mutating call. In **Auto** mode, only destructive
//! Bash commands require confirmation.
//!
//! | Tool | Effect | Description |
//! |------|--------|-------------|
//! | `Read` | Read-only | Read a file (with optional line range) |
//! | `Write` | Mutating | Create or overwrite a file |
//! | `Edit` | Mutating | Targeted text replacement within a file |
//! | `Delete` | Mutating | Delete a file or directory |
//! | `Bash` | Varies | Run a shell command |
//! | `Grep` | Read-only | Search for patterns across files (ripgrep) |
//! | `Glob` | Read-only | List files matching a glob pattern |
//! | `WebFetch` | Read-only | Fetch a URL and return its text content |
//! | `Think` | Internal | Extended reasoning step (no side effects) |
//! | `MemoryWrite` | Mutating | Append a fact to a memory file |
//! | `ListSkills` | Read-only | List available skills |
//! | `ActivateSkill` | Internal | Load a skill's instructions into context |
//! | `InvokeAgent` | Varies | Delegate a task to a named sub-agent |
//! | `ListFiles` | Read-only | List directory contents |
//! | `AskUser` | Interactive | Ask the user a clarifying question |
//!
//! ---
//!
//! ## ACP server (editor integration)
//!
//! Koda implements the [Agent Client Protocol](https://agentclientprotocol.org)
//! over stdio JSON-RPC 2.0. This lets editors connect to Koda as a local agent
//! without network setup.
//!
//! ```bash
//! # Start the server (editors launch this automatically)
//! koda server --stdio
//! ```
//!
//! The protocol lifecycle:
//!
//! ```text
//! Editor → initialize           (negotiate protocol version)
//! Koda   ← InitializeResponse
//!
//! Editor → session/new          (create a session)
//! Koda   ← NewSessionResponse   (returns session_id)
//!
//! Editor → session/prompt       (send a user message)
//! Koda   ← [stream of session/update events]
//! Koda   ← PromptResponse       (turn complete)
//!
//! Editor → Cancel               (optional — aborts the running turn)
//! ```
//!
//! Each line on stdin/stdout is a complete, self-contained JSON-RPC object.
//! For VS Code, Zed, and other editors, see your editor's extension docs for
//! how to configure a local ACP agent.
//!
//! ---
//!
//! ## Configuration files
//!
//! Everything lives in `~/.config/koda/`:
//!
//! | Path | Content |
//! |------|---------|
//! | `db/koda.db` | SQLite — sessions, messages, settings, API keys, input history |
//! | `logs/koda.log` | Rolling daily tracing log (not shown in the TUI) |
//! | `agents/` | Global custom agent JSON definitions |
//! | `skills/` | Global custom skill markdown files |
//! | `memory.md` | Global memory (injected into all system prompts) |
//!
//! Project-level overrides live in `.koda/` at your project root and take
//! priority over global config:
//!
//! | Path | Content |
//! |------|---------|
//! | `.koda/agents/` | Project-specific agent definitions |
//! | `.koda/skills/` | Project-specific skills |
//! | `MEMORY.md` | Project memory (also checks `CLAUDE.md`, `AGENTS.md`) |
//!
//! ---
//!
//! ## Privacy and data
//!
//! Koda has **zero telemetry**. No usage data, crash reports, or analytics
//! are collected or transmitted anywhere.
//!
//! - Conversations are stored **only** in your local SQLite database
//! - API keys are stored locally in the same database (file mode 0600)
//! - The only network traffic is your LLM API calls to the provider you chose
//! - Version checks query crates.io only (no Koda-specific server)
//! - You can audit every byte sent to the model by reading the DB directly

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
