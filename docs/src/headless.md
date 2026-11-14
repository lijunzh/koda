# Headless mode

`koda exec` runs a single prompt, prints the answer, and exits, as Codex's
`codex exec` does. No TUI: the assistant's reply streams to stdout; tool status
goes to stderr.

```bash
koda exec "what does this codebase do?"

# Read the prompt from stdin: "-", or no prompt with stdin piped
koda exec - < my_question.txt
git diff HEAD~1 | koda exec
cat error.log | koda exec

# With a model override
koda exec --model gemini-flash-latest "explain this"

# Capture just the assistant reply (tool status stays on stderr)
koda exec "list exported functions in lib.rs" > functions.txt

# Also write the last reply to a file
koda exec -o last.txt "summarize the diff"

# The client protocol, one JSON line per message
koda exec --json "fix the failing test"
```

Every `koda` flag (`--model`, `--provider`, `--mode`, `-s`, `--mcp-config`, …)
works with it.

`koda` without `exec` opens the TUI. `koda -p`, `--output-format` and a prompt
given to `koda` itself (`koda "..."`) were removed in 0.7.0 and stop with a
pointer here; `koda` with stdin piped stops too, as `codex` does.

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
- **Rejects** all destructive operations: `rm -rf`, `git push --force`,
  etc.
- **Skips** AskUser questions (prints to stderr, continues with empty answer)

The sandbox enforces credential protection and write restrictions
regardless of trust mode.

### Read-only runs

`--mode plan` makes a run read-only, for a job that should only look,
such as summarizing a mailbox or reviewing a branch:

```bash
koda exec --mode plan "Anything in my inbox I need to deal with?"
```

- **Runs** tools that read: Read, Grep, Glob, web fetch and search, Bash
  commands Koda judges read-only, and MCP tools whose server marks them
  `readOnlyHint`.
- **Refuses** everything else without asking: Write, Edit, other Bash
  commands and MCP tools without the mark. The model is told the call was
  refused and that it should say what it would do instead.
- **Keeps the project read-only in the kernel sandbox** too, so a command
  wrongly judged read-only still can't change it. Temp and cache
  directories stay writable.

An MCP server whose tools don't carry `readOnlyHint` can't be used in a
read-only run. Codex's `exec` (read-only sandbox by default), and the plan
modes of Claude Code, Gemini CLI and Qwen Code in their one-shot runs, work the
same way.

## Output formats

**Text** (`koda exec`): the assistant's reply streams to stdout exactly as
typed. Tool call summaries go to stderr.

**The client protocol** (`koda exec --json`): stdout carries only JSON-RPC 2.0
notifications, one per line, described below. This is the format for programs
and the eval harness:

```bash
koda exec --json "fix the failing test" \
  | jq -c 'select(.method == "item/completed") | .params.item'
```

`--output-format stream-json`, the protocol of Koda 0.6, and `koda -p
--output-format json`'s one summary object were removed in 0.7.0. `koda exec
--json` replaces both: its last line, `exec/summary`, carries what the summary
object did, in camelCase.

## The client protocol

`koda exec --json` prints the notifications of Koda's client protocol, the one
every Koda client will speak. It follows Codex's app-server protocol: JSON-RPC
2.0 messages (Koda keeps the `"jsonrpc": "2.0"` member), `noun/verb` method
names and camelCase fields. A conversation is a *session*; a *turn* is one
prompt and everything Koda does for it; a turn is made of *items* that start,
may stream, and complete. [`protocol-schema.json`](protocol-schema.json) is
generated from the protocol's types, and CI fails if the two differ.

```json
{"jsonrpc":"2.0","method":"session/started","params":{"session":{"id":"a3f8bc12-…","model":"claude-haiku-5-5","provider":"anthropic","contextWindow":200000,"mode":"auto","tools":["Read","Grep","…"]},"kodaVersion":"0.7.0","protocolVersion":2}}
{"jsonrpc":"2.0","method":"turn/started","params":{"sessionId":"a3f8bc12-…","turn":{"id":"7d1e…","status":"inProgress","error":null,"usage":null,"durationMs":null}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"userMessage","id":"user-1","text":"Why does the test fail?","clientMessageIds":[]}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"userMessage","id":"user-1","text":"Why does the test fail?","clientMessageIds":[]}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"toolCall","id":"call_1","tool":"Bash","arguments":{"command":"cargo test"},"status":"inProgress","output":null,"detail":null}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"toolCall","id":"call_1","tool":"Bash","arguments":{"command":"cargo test"},"status":"failed","output":"Exit code: 101 | …","detail":null}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"agentMessage","id":"msg-1","text":""}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"agentMessage","id":"msg-1","text":"The test fails because …"}}}
{"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"a3f8bc12-…","turn":{"id":"7d1e…","status":"completed","error":null,"usage":{"inputTokens":18234,"cachedInputTokens":15012,"cacheCreationTokens":0,"outputTokens":912,"reasoningTokens":0,"modelCalls":3},"durationMs":8421}}}
{"jsonrpc":"2.0","method":"exec/summary","params":{"sessionId":"a3f8bc12-…","success":true,"…":"…"}}
```

A run prints `session/started`, one turn, and `exec/summary`, in that order.
Between the first two may come a `warning` from starting the session: a local
server that doesn't report its context window gets one saying what Koda
assumed. Every message after the first carries `sessionId`; every message in a
turn carries `turnId`.

`koda exec` is a client like any other: it starts Koda's host in process and
opens a session with `approvalPolicy: never`, so the host answers the requests
below as described, and Ctrl+C sends `turn/interrupt`.

### Notifications

| Method | Carries | Notes |
|--------|---------|-------|
| `session/started` | `session` (`id`, `model`, `provider`, `contextWindow`, `mode`, `tools`), `kodaVersion`, `protocolVersion` | Always first. Stop on a `protocolVersion` you don't support |
| `turn/started` | `turn`, in progress | |
| `item/started`, `item/completed` | `item`, `parentItemId` on a sub-agent's items | Every item that starts completes, once |
| `item/agentMessage/delta` | `itemId`, `delta` | Only with `--json-deltas` |
| `item/reasoning/textDelta` | `itemId`, `delta` | Only with `--json-deltas` |
| `item/toolCall/outputDelta` | `itemId`, `delta`, `stream` (`stdout` or `stderr`) | A line of a running command. Only with `--json-deltas` |
| `turn/plan/updated` | `plan`: `{step, status}` | The model's todo list, whole. `status` is `pending`, `inProgress` or `completed` |
| `turn/loopGuard` | `action`, `detail` | The model repeated itself. `action` is `feedback`, or `stop` when it didn't change course. Koda's own |
| `error` | `error.message`, `willRetry`, `retry` | With `willRetry`, Koda tries the model call again and the turn goes on. `retry` has `reason` (`rateLimit`, `network`, `serverError`, `streamFailed`, `contextOverflow`, `emptyResponse`), `attempt` (the one that failed, from 1), `maxAttempts` and `delayMs` |
| `warning` | `message` | For people |
| `serverRequest/resolved` | `requestId` | The request was answered. In `koda exec`, right after each request |
| `turn/completed` | `turn`: `status`, `error`, `usage`, `durationMs` | Exactly one per turn, after any retries. `status` is `completed`, `interrupted` or `failed` |
| `exec/summary` | the run as a whole | `koda exec` only, always last |

### Items

| `type` | Fields | Notes |
|--------|--------|-------|
| `userMessage` | `id`, `text`, `clientMessageIds` | What the user said, as the model received it: a turn's prompt, or steering text sent during it (texts that arrived together are one item) |
| `agentMessage` | `id`, `text` | Text is empty when it starts and complete when it completes |
| `reasoning` | `id`, `text` | Where the provider shows reasoning |
| `toolCall` | `id`, `tool`, `arguments`, `status`, `output`, `detail` | One item for every tool, named by `tool`. `status` ends as `completed`, `failed` (it ran and failed, or a command exited non-zero), `declined` (refused at an approval, by `--mode plan` or by a sub-agent's limits) or `cancelled`. `output` is what the model read. `detail` is the one-line description Koda showed when it asked about or blocked the call |
| `contextCompaction` | `id`, `trigger`, `status`, `contextPercent`, `messagesRemoved`, `summaryTokens`, `reason` | `trigger` is `threshold`, `overflow`, `window` or `idle`. `status` ends as `completed`, `skipped` or `failed` (`reason` says why) |

Codex has an item per kind of call (`commandExecution`, `fileChange`,
`mcpToolCall`, …). Koda's tools share one shape, so it has one; typed fields
such as a diff will be added when a client needs them.

### Requests

Koda asks with JSON-RPC requests, which have an `id`:

| Method | Carries | Answer |
|--------|---------|--------|
| `item/toolCall/requestApproval` | `itemId`, `tool`, `detail`, `effect`, `allowForSession` | `decision`: `approve`, `approveForSession`, `reject` or `rejectWithFeedback` |
| `item/tool/requestUserInput` | `itemId`, `question`, `options` | `answer` |
| `turn/loopCap/requestDecision` | `cap`, `recentTools` | `decision`: `continue50`, `continue200` or `stop` |

In `koda exec` nothing reads an answer: Koda answers its own, as in
[Trust mode](#trust-mode-in-headless-mode), and prints `serverRequest/resolved`
with the request's id right after it. The answer shows in the call's item: a
refused call completes as `declined`.

### Usage

`turn/completed`'s `usage` counts this turn alone, however it ended, and
includes sub-agents, compaction and retries. A model call cut off before the
provider reported usage counts for nothing, so a cancelled or failed turn
reports the calls that finished. `exec/summary`'s `usage` covers the stored
session, earlier turns of a resumed one included, and `usageByAgent` splits it
with the session's own calls under `main`.

**A cancelled turn** gets five seconds to stop. A sub-agent stops at once and
gives back its workspace, but a Bash command the turn itself is running doesn't
listen, so a turn waiting on one is dropped where it is. It still closes with
one `turn/completed` with status `interrupted`, but `exec/summary` then has
`success: false` and an `error`, and the run exits 1, since work may have been
left unfinished.

### Reading what you don't know

- **Skip an unknown notification, or an unknown item type.** New ones can appear
  without a version change.
- **Never skip a request** once a client can answer: a skipped request leaves
  Koda waiting. Refuse one you can't handle. (`koda exec` answers its own.)
- **Stop on a `protocolVersion` you don't support.** An incompatible change
  raises it.

## File attachment in headless mode

The `@file` syntax works in headless mode too:

```bash
koda exec "review @src/auth.rs and @tests/auth_test.rs"
```

## Resuming a session in headless mode

```bash
# The session id from session/started or exec/summary
koda exec -s a3f8bc12-1587-4c6d-a855-46183ca84fba "run the failing tests and fix them"
```
