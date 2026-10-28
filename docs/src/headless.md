# Headless mode

Headless mode runs a single prompt, prints the answer, and exits.
No TUI — the assistant's reply streams to stdout; tool status goes to stderr.

```bash
# Positional prompt (shortest form)
koda "what does this codebase do?"

# Explicit -p flag
koda -p "fix the failing tests"

# Read prompt from stdin (use "-" literally)
koda -p - < my_question.txt

# Pipe into koda — stdin is auto-detected when not a TTY
git diff HEAD~1 | koda
cat error.log | koda
echo "review auth.rs" | koda

# With a model override
koda "explain this" --model gemini-flash

# Capture just the assistant reply (tool status stays on stderr)
koda "list exported functions in lib.rs" > functions.txt
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Turn completed successfully |
| `1` | Error (API failure, bad config, …) |

## Trust mode in headless mode

There is no human to approve tool calls. Koda applies a **headless
policy** — more restrictive than interactive Auto mode:

- **Approves** read-only tools: Read, Grep, Glob, WebFetch
- **Approves** safe write tools: Write, Edit (files within the project)
- **Rejects** all destructive operations: `Delete` tool, `rm -rf`,
  `git push --force`, etc.
- **Skips** AskUser questions (prints to stderr, continues with empty answer)

The sandbox enforces credential protection and write restrictions
regardless of trust mode.

## Output formats

`--output-format text` (default) — streams the assistant's reply to stdout
exactly as typed. Tool call summaries go to stderr.

`--output-format json` — stdout carries exactly one JSON object, printed
after the turn ends. The streamed reply and tool status go to stderr.

```json
{
  "type": "run_summary",
  "success": true,
  "error": null,
  "response": "The exported functions are …",
  "session_id": "a3f8bc12-…",
  "provider": "anthropic",
  "model": "claude-sonnet-5-5",
  "turn_end": { "kind": "complete" },
  "elapsed_ms": 8421,
  "usage": {
    "prompt_tokens": 18234, "completion_tokens": 912,
    "cache_read_tokens": 15012, "cache_creation_tokens": 0,
    "thinking_tokens": 0, "api_calls": 3
  },
  "usage_by_agent": { "main": { "…": "same fields as usage" } },
  "tool_calls": { "Read": 2, "Grep": 1 },
  "sub_agent_tool_calls": {}
}
```

`usage` is summed from the session's stored messages, so it includes
sub-agent calls and is still accurate when the turn errors or is cancelled.
`turn_end` is `null` if the turn never ended (for example, a setup failure).

`--output-format stream-json` — stdout carries newline-delimited JSON only:
one line per engine event as it happens, then the same summary object as
the last line. Every line has a `type` field. Event lines use the engine's
event names (`turn_start`, `text_delta`, `tool_call_start`,
`tool_call_result`, `footer`, `turn_end`, …); the summary line is
`run_summary`. Nothing else is written to stdout, which makes this the
format for scripts and the eval harness:

```bash
koda -p "fix the failing test" --output-format stream-json \
  | jq -c 'select(.type == "tool_call_start" or .type == "run_summary")'
```

## File attachment in headless mode

The `@file` syntax works in headless mode too:

```bash
koda "review @src/auth.rs and @tests/auth_test.rs"
```

## Resuming a session in headless mode

```bash
# Note the session ID shown in the TUI status bar, then continue from a script
koda -s a3f8bc "run the failing tests and fix them"
```
