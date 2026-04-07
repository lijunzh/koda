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

## Approval in headless mode

There is no human to approve tool calls. Koda automatically:

- **Approves** read-only tools: Read, Grep, Glob, WebFetch
- **Approves** safe write tools: Write, Edit (files only)
- **Rejects** destructive Bash commands (`rm -rf`, `git push --force`, …)
- **Skips** AskUser questions (prints to stderr, continues with empty answer)

Rejected actions are printed to stderr. The turn still completes with
exit code 0 unless the API itself errors.

## Output formats

`--output-format text` (default) — streams the assistant's reply to stdout
exactly as typed. Tool call summaries go to stderr.

`--output-format json` — emits a single JSON object after the turn ends:

```json
{
  "success": true,
  "response": "The exported functions are …",
  "session_id": "a3f8bc12-…",
  "model": "claude-sonnet-4-6"
}
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
