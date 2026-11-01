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
koda "explain this" --model gemini-flash-latest

# Capture just the assistant reply (tool status stays on stderr)
koda "list exported functions in lib.rs" > functions.txt
```

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Turn completed successfully |
| `1` | Error (API failure, bad config, a cancelled turn that wouldn't stop, …) |

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

`usage` is summed from stored messages, including those of the session's
sub-agents, so it is still accurate when the turn errors or is cancelled. On a
resumed session it covers every turn stored so far. `usage_by_agent` splits it,
with the session's own calls under `main`. For what one turn used, read
`turn_summary` below.
`turn_end` is `null` if the turn never ended (for example, a setup failure).

`--output-format stream-json` — stdout carries newline-delimited JSON only: the
wire protocol below. This is the format for scripts and the eval harness:

```bash
koda -p "fix the failing test" --output-format stream-json \
  | jq -c 'select(.type == "tool_call_start" or .type == "run_summary")'
```

## Wire protocol

`stream-json` is Koda's public protocol, version 1. It is a projection of the
engine's events, so the engine can change without changing what clients read.
[`wire-schema.json`](wire-schema.json) is generated from it, and CI fails if the
two differ.

A run writes `hello`, `session_ready`, one turn, and `run_summary`, in that
order. `koda -p` is always one turn per process.

```json
{"type":"hello","protocol_version":1,"koda_version":"0.5.1","capabilities":["one_shot","self_answer"]}
{"type":"session_ready","session_id":"a3f8bc12-…","provider":"anthropic","model":"claude-haiku-5-5","context_window":200000,"trust":"auto","tools":["Read","Grep","…"]}
{"type":"turn_start","session_id":"a3f8bc12-…","turn_id":"7d1e…"}
{"type":"tool_call_start","session_id":"a3f8bc12-…","turn_id":"7d1e…","id":"call_1","name":"Bash","args":{"command":"cargo test"}}
{"type":"tool_call_result","session_id":"a3f8bc12-…","turn_id":"7d1e…","id":"call_1","name":"Bash","output":"Exit code: 101 | …","outcome":"error"}
{"type":"turn_summary","session_id":"a3f8bc12-…","turn_id":"7d1e…","usage":{"prompt_tokens":18234,"completion_tokens":912,"cache_read_tokens":15012,"cache_creation_tokens":0,"thinking_tokens":0,"api_calls":3},"elapsed_ms":8421}
{"type":"turn_end","session_id":"a3f8bc12-…","turn_id":"7d1e…","reason":{"kind":"complete"}}
{"type":"run_summary","session_id":"a3f8bc12-…","success":true,"…":"…"}
```

**Ids.** `session_id` is on every line except `hello`, which comes before a
session exists. `turn_id` is on every line from `turn_start` to `turn_end`.
`parent_id` is on a sub-agent's lines and holds the id of the tool call that
started it, so two sub-agents running at once can be told apart. Lines without
it are the top-level agent's own.

**Capabilities.** `one_shot` means one turn per process. `self_answer` means the
process answers its own requests, as in [Trust mode](#trust-mode-in-headless-mode):
requests on stdout are a record of what was asked, and nothing reads a reply.
Ignore capability names you don't know.

| Line | Carries | Notes |
|------|---------|-------|
| `hello` | `protocol_version`, `koda_version`, `capabilities` | Always first |
| `session_ready` | `provider`, `model`, `context_window`, `trust`, `tools` | Before any turn |
| `turn_start` | | |
| `text_delta`, `text_done` | `text` | |
| `thinking_start`, `thinking_delta`, `thinking_done` | `text` | |
| `tool_call_start` | `id`, `name`, `args` | |
| `tool_output_line` | `id`, `line`, `is_stderr` | Live Bash output |
| `tool_call_result` | `id`, `name`, `output`, `outcome` | `outcome` is `success` or `error`; a command that exits non-zero or times out is `error`. Rejected, blocked and cancelled calls, and calls made by sub-agents, have no result line in version 1 |
| `sub_agent_start` | `agent_name` | |
| `todo_update` | `items`, `diff` | |
| `approval_request` | `id`, `tool_name`, `detail`, `preview`, `effect` | Interactive |
| `ask_user_request` | `id`, `question`, `options` | Interactive |
| `loop_cap_request` | `id`, `cap`, `recent_tools` | Interactive |
| `action_blocked` | `tool_name`, `detail`, `preview` | |
| `context_usage` | `used`, `max` | |
| `context_window_changed` | `tokens` | |
| `compaction` | `trigger`, `status` | See below |
| `retry` | `reason`, `attempt`, `max_attempts`, `delay_ms` | `reason` is `rate_limit`, `network`, `context_overflow` or `empty_response`. `attempt` is the one that just failed, counting from 1 |
| `loop_guard` | `action`, `detail` | `action` is `feedback`, or `stop` when the model ignored it |
| `info`, `warn`, `error` | `message` | For people. Don't parse them |
| `turn_summary` | `usage`, `elapsed_ms` | Just before `turn_end` |
| `turn_end` | `reason` | Exactly one per turn, after any retries. `reason.kind` is `complete`, `cancelled` or `error` |
| `run_summary` | The object shown above | Always last |

**`compaction`.** `trigger` is `threshold` (context passed the auto-compact
level), `overflow` (the provider rejected the request), `window` (history was
dropped unsummarized because the estimate passed the window) or `idle` (old tool
results were cleared after a gap). `status` adds its own fields:

| `status` | Fields | Meaning |
|----------|--------|---------|
| `started` | `context_percent` | Exactly one other status follows. `window` and `idle` don't send it |
| `completed` | `messages_removed`, `summary_tokens` | `summary_tokens` is `null` when there is no summary |
| `skipped` | `reason` | `pending_tool_calls`, `too_short` or `history_too_large` |
| `failed` | `error` | The summary call failed |

**`turn_summary`.** `usage` counts this turn alone, however it ended, and
includes sub-agents, compaction and retries. On a resumed session it doesn't
include earlier turns. A model call cut off before the provider reported usage
counts for nothing, so a cancelled or failed turn reports the calls that
finished.

**A cancelled turn** gets five seconds to stop. One that hasn't stopped by then,
such as a sub-agent waiting on a model call, is dropped where it is. It still
closes with one `turn_summary` and one `turn_end` of kind `cancelled`, but
`run_summary` then has `success: false` and an `error`, and the run exits 1, since
work may have been left unfinished.

### Reading unknown types

New line types can appear without a version change, so a reader has to decide
what to do with a `type` it doesn't know:

- **Skip an unknown notification.** Most lines just report something.
- **Never skip an interactive request.** A skipped request leaves its sender
  waiting forever. Once a client can reply, it must refuse a request type it
  can't handle, and a sender must fail a command it doesn't support. In version
  1 the requests are `approval_request`, `ask_user_request` and
  `loop_cap_request`, and `self_answer` means no reply is expected.
- **Stop on a `protocol_version` you don't support.** An incompatible change
  raises it.

Decode with this in mind. Rust's `#[non_exhaustive]` does nothing here, because
it governs matches in code and not JSON: serde rejects an unknown tag with
`unknown variant` unless the reader handles it.

## File attachment in headless mode

The `@file` syntax works in headless mode too:

```bash
koda "review @src/auth.rs and @tests/auth_test.rs"
```

## Resuming a session in headless mode

```bash
# The full session_id from --output-format json
koda -s a3f8bc12-1587-4c6d-a855-46183ca84fba "run the failing tests and fix them"
```
