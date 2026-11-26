# Headless mode

`koda exec` runs a single prompt, prints the answer, and exits, as Codex's
`codex exec` does. No TUI: the assistant's reply streams to stdout; tool status
goes to stderr. That status is drawn from the [client protocol](#the-client-protocol),
as `codex exec`'s is: each tool call and its output, a sub-agent's calls
indented under its `InvokeAgent` call with its notices, changes to the
model's plan as `[todos] +added ~changed -removed`, warnings, and a last line
with the turn's output tokens, time and speed.

## Goals

As `claude -p "/goal …"` does, a prompt of `/goal <objective>` sets the
session's goal instead of sending a message, and the run follows the goal's
turns until it's no longer active: complete, blocked, or out of budget
([`/goal`](./commands.md#goal)). The last turn's reply is the run's, and the
stream carries each `session/goal/updated` (and, with
`goal_judge = "evaluator"`, each `session/goal/judged`); text output shows
them as `Goal (…)` and `Goal check:` lines on stderr. Ctrl+C stops the turn
running, or the wait between turns. A goal on hold after eight turns that
call no tool ends the run, since nobody is there to send the next message. A
turn that `turn_token_budget` stopped doesn't: the goal stays active and the
run goes on with its next turn, as in the TUI, so `turn_token_budget` caps each
turn and `max_goal_token_budget` the goal. A goal that never finishes, because the model keeps working without marking it
complete, runs until its budget is used up: set one with
`-c max_goal_token_budget=…`, or give the run a time limit of its own.

```bash
koda exec '/goal every test in tests/ passes' -c goal_judge=evaluator
```

## Background commands

As `claude -p` does, a run stays open while a command the model started with
`Bash` `background: true` is still running. When the command exits, Koda tells
the model in a turn of its own, and that turn's reply ends the run (or it
starts another command and the run waits again). With text output, stderr says
`Waiting for background commands to finish` and names them. Ctrl+C while
waiting stops the wait, and the commands with it.

Nobody attends a run, so its background commands have a time limit, as Claude
Code's do in `claude -p`: 10 minutes, or the `timeout` the model passes with
`background`, up to 2 hours. A command that reaches it is stopped, and the
model is told why, so it can start it again with a longer `timeout`. A dev
server left running therefore holds a run for at most its limit.
`background_time_limit_minutes` changes the 10, and `0` takes the limit off,
so a run then waits as long as the command runs. A command the model stops
itself ends the wait with no further turn.

Only commands started with `background: true` hold the run. Processes an
ordinary command leaves running, such as a server it started with `&`, are
tracked and can be stopped like background commands, but the run doesn't wait
for them, as Codex's `exec` doesn't: they stop when the run ends.

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

# A final reply a script can parse: JSON matching a schema
koda exec --output-schema result.schema.json "did the tests pass?"
```

## Structured output

`--output-schema FILE` holds the last reply to a JSON Schema, as `codex exec
--output-schema` and `claude -p --json-schema` do. The reply on stdout is then
the JSON value alone, and the run exits 1 if it doesn't match:

```bash
cat > result.schema.json <<'JSON'
{
  "type": "object",
  "properties": { "passed": { "type": "boolean" }, "failures": { "type": "integer" } },
  "required": ["passed", "failures"],
  "additionalProperties": false
}
JSON
koda exec --output-schema result.schema.json "run the tests and report" | jq .passed
```

The turn runs as usual, tools and all, with the schema in the prompt. If its
last reply doesn't match, Koda asks once more with the schema enforced by the
API (Claude's structured outputs, a Chat Completions `response_format`,
Gemini's `responseJsonSchema`) and no tools, since llama.cpp refuses a schema
beside them. A schema Koda can't check stops the run before the model is
called. Over the protocol, `turn/start` takes the same schema as
`outputSchema`.

Every `koda` flag (`--model`, `--provider`, `--mode`, `-s`, `--mcp-config`, …)
works with it.

`koda` without `exec` opens the TUI, and `koda "prompt"` opens it with that
prompt, as `codex "prompt"` does; `koda` with stdin piped stops with "stdin is
not a terminal", as `codex` does. Coming from 0.6's `koda -p`, see [Upgrading
to 0.7](./upgrading-0.7.md#running-koda).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Turn completed successfully |
| `1` | Error (API failure, bad config, a cancelled turn that wouldn't stop, a turn that spent its [`turn_token_budget`](./configuration.md#limiting-what-a-turn-spends), …), with the reason on stderr and, with `--json`, in `exec/summary`'s `error` |

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

The last line, `exec/summary`, sums up the run (the session, how the turn
ended, its time and usage), in camelCase.

## The client protocol

`koda exec --json` prints the notifications of Koda's client protocol, the one
every Koda client will speak. It follows Codex's app-server protocol: JSON-RPC
2.0 messages (Koda keeps the `"jsonrpc": "2.0"` member), `noun/verb` method
names and camelCase fields. A conversation is a *session*; a *turn* is one
prompt and everything Koda does for it; a turn is made of *items* that start,
may stream, and complete. [`protocol-schema.json`](protocol-schema.json) is
generated from the protocol's types, and CI fails if the two differ.

```json
{"jsonrpc":"2.0","method":"session/started","params":{"session":{"id":"a3f8bc12-…","preview":"","createdAt":1794700000,"updatedAt":1794700000,"cwd":"/home/me/project","model":"claude-haiku-5-5","modelProvider":"anthropic","contextWindow":200000,"mode":"auto","tools":["Read","Bash","…"]},"kodaVersion":"0.7.14","protocolVersion":4}}
{"jsonrpc":"2.0","method":"turn/started","params":{"sessionId":"a3f8bc12-…","turn":{"id":"7d1e…","status":"inProgress","error":null,"usage":null,"durationMs":null,"startedAt":1794700001,"completedAt":null}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"userMessage","id":"user-3f9a2c1e-1","content":[{"type":"text","text":"Why does the test fail?"}],"clientUserMessageIds":[]}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"userMessage","id":"user-3f9a2c1e-1","content":[{"type":"text","text":"Why does the test fail?"}],"clientUserMessageIds":[]}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"toolCall","id":"call_1","tool":"Bash","arguments":{"command":"cargo test"},"status":"inProgress","output":null,"detail":null}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"toolCall","id":"call_1","tool":"Bash","arguments":{"command":"cargo test"},"status":"failed","output":"Exit code: 101 | …","detail":null}}}
{"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"agentMessage","id":"msg-3f9a2c1e-2","text":""}}}
{"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"a3f8bc12-…","turnId":"7d1e…","item":{"type":"agentMessage","id":"msg-3f9a2c1e-2","text":"The test fails because …"}}}
{"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"a3f8bc12-…","turn":{"id":"7d1e…","status":"completed","error":null,"usage":{"inputTokens":18234,"cachedInputTokens":15012,"cacheWriteInputTokens":0,"outputTokens":912,"reasoningOutputTokens":0,"modelCalls":3},"durationMs":8421,"startedAt":1794700001,"completedAt":1794700009}}}
{"jsonrpc":"2.0","method":"exec/summary","params":{"sessionId":"a3f8bc12-…","success":true,"…":"…"}}
```

A run prints `session/started`, one turn, and `exec/summary`, in that order;
a turn Koda starts for the model's [background commands](#background-commands)
comes after the first, with `exec/summary` still last.
Between the first two may come a `warning` from starting the session: a local
server that doesn't report its context window gets one saying what Koda
assumed. Every message after the first carries `sessionId`; every message in a
turn carries `turnId`.

[`koda serve --stdio`](serve.md) serves the same protocol to another program,
which sends the requests and answers Koda's.

`koda exec` is a client like any other: it starts Koda's host in process and
opens a session with `approvalPolicy: never`, so the host answers the requests
below as described, and Ctrl+C sends `turn/interrupt`.

### Notifications

| Method | Carries | Notes |
|--------|---------|-------|
| `session/started` | `session` (`id`, `name`, `preview`, `createdAt`, `updatedAt`, `cwd`, and while open `model`, `modelProvider`, `contextWindow`, `mode`, `tools`), `kodaVersion`, `protocolVersion` | Always first. Stop on a `protocolVersion` you don't support. Times are Unix seconds; `name` is the title, `preview` the first prompt |
| `turn/started` | `turn`, in progress, with `startedAt` | |
| `item/started`, `item/completed` | `item`, `parentItemId` on a sub-agent's items, `startedAtMs` or `completedAtMs` | Every item that starts completes, once. The times are Unix milliseconds, as Codex's; a stored turn's items keep both |
| `item/agentMessage/delta` | `itemId`, `delta` | Only with `--json-deltas` |
| `item/reasoning/textDelta` | `itemId`, `delta` | Only with `--json-deltas` |
| `item/toolCall/outputDelta` | `itemId`, `delta`, `stream` (`stdout` or `stderr`) | A line of a running command, as Codex caps them: at most 10,000 per call, each at most 8 KiB (a longer one is cut and ends with `…`). Only with `--json-deltas` |
| `item/toolCall/progress` | `itemId`, `message` | A notice about a call, such as a sub-agent reaching its turn limit, or a skill restricting the tools. Codex's `item/mcpToolCall/progress` |
| `session/tokenUsage/updated` | `tokenUsage`: `total`, `last`, `modelContextWindow`, `byAgent` | After each model call, as Codex's `thread/tokenUsage/updated`, and once when a session is resumed, with what it had stored, as Codex replays a thread's. `total` adds up every call since the session was created, sub-agents' and compactions' included; `byAgent` splits it by agent (the session's own under `main`), with each agent's `modelCalls`; `last` is the session's latest call, and its `totalTokens` is how full the context was. Each counts `totalTokens`, `inputTokens` (cached tokens included), `cachedInputTokens`, `cacheWriteInputTokens`, `outputTokens` and `reasoningOutputTokens` |
| `turn/plan/updated` | `plan`: `{step, status}` | The model's todo list, whole. `status` is `pending`, `inProgress` or `completed` |
| `turn/diff/updated` | `diff` | What the turn's `Edit` and `Write` calls have changed so far, as one unified diff in git's format, paths relative to the session's folder, as Codex's. Sent after each edit that changes it and again just before `turn/completed`. Each file is compared with its content when the turn first edited it; changes made by commands and by sub-agents aren't in it. Empty once the turn's edits are undone, or after an edit Koda couldn't track |
| `turn/loopGuard` | `action`, `detail` | The model repeated itself. `action` is `feedback`, or `stop` when it didn't change course. Koda's own |
| `error` | `error.message`, `willRetry`, `retry` | With `willRetry`, Koda tries the model call again and the turn goes on. `retry` has `reason` (`rateLimit`, `network`, `serverError`, `streamFailed`, `contextOverflow`, `emptyResponse`), `attempt` (the one that failed, from 1), `maxAttempts` and `delayMs` |
| `warning` | `message` | For people |
| `serverRequest/resolved` | `requestId` | The request was answered. In `koda exec`, right after each request |
| `turn/completed` | `turn`: `status`, `error`, `usage`, `durationMs`, `startedAt`, `completedAt` | Exactly one per turn, after any retries. The times are Unix seconds, as Codex's. `status` is `completed`, `interrupted` or `failed`. A failed turn's `error` may carry a `code`: `tokenBudgetSpent` when it spent its [`turn_token_budget`](./configuration.md#limiting-what-a-turn-spends) |
| `exec/summary` | the run as a whole | `koda exec` only, always last |

### Items

| `type` | Fields | Notes |
|--------|--------|-------|
| `userMessage` | `id`, `content`, `clientUserMessageIds` | What the user sent, as `turn/start` or `turn/steer` took it: `content` is the list of parts (`text`, `image`, `localImage`, `skill`, `mention`; see [What a prompt is](serve.md#what-a-prompt-is)). A turn's prompt, or steering text sent during it (texts that arrived together are one item) |
| `agentMessage` | `id`, `text`, `phase` | Text is empty when it starts and complete when it completes. On completion, `phase` says what the text was to the turn, as Codex's agent message does: `commentary` when the same model response also asked for tools (a note on the way), `final_answer` when it didn't (the answer). Codex's model sets it; Koda's server works it out from the response. Left out while the text streams and when the response was cut short (stopped, failed or retried) |
| `reasoning` | `id`, `text` | Where the provider shows reasoning |
| `toolCall` | `id`, `tool`, `kind`, `arguments`, `status`, `output`, `exitCode`, `durationMs`, `detail`, `changes`, `commandActions`, `progress` | One item for every tool, named by `tool`. `kind` is what sort of tool it is, from the Agent Client Protocol's list (`read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `switch_mode`, `other`), so a client can draw a call without knowing every tool's name; an MCP tool is `other`. `status` ends as `completed`, `failed` (it ran and failed, or a command exited non-zero), `declined` (refused at an approval, by `--mode plan` or by a sub-agent's limits) or `cancelled`. `output` is what the model read. On completion, `exitCode` is the exit code of the command a `Bash` or `Cargo` call ran (absent for other tools, or when the command timed out, was stopped or went to the background), and `durationMs` how long the tool ran, not counting a wait for approval (absent when it never ran), as Codex's items have them. `detail` is the one-line description Koda showed when it asked about or blocked the call. `changes` is what an `Edit` or `Write` changes, from the call's start whether or not anyone was asked, each `{path, kind, diff}` as Codex's `fileChange` item has them (`kind` is `{"type": "add"}` with the new file as `diff`, or `{"type": "update"}` with a unified diff). `commandActions` says what a `Bash` call's command does, from the call's start, one entry per command in it, as Codex's `commandExecution` item has them: `{"type": "read", command, name, path}`, `{"type": "listFiles", command, path}`, `{"type": "search", command, query, path}` or `{"type": "unknown", command}`; a best-effort reading of the words, for showing, not for deciding anything. `progress` lists the call's notices, on completion. The last four are left out when empty |
| `contextCompaction` | `id`, `trigger`, `status`, `contextPercent`, `messagesRemoved`, `summaryTokens`, `reason` | `trigger` is `threshold`, `overflow`, `window`, `manual` (asked for with `session/compact/start`) or `postTurn` (the turn's last reply left the context past the after-turn threshold). `status` ends as `completed`, `skipped` or `failed` (`reason` says why) |

Codex has an item per kind of call (`commandExecution`, `fileChange`,
`mcpToolCall`, …). Koda's tools share one shape, so it has one, and adds
typed fields such as `changes` when a client needs them.

### Requests

Koda asks with JSON-RPC requests, which have an `id`:

| Method | Carries | Answer |
|--------|---------|--------|
| `item/toolCall/requestApproval` | `itemId`, `tool`, `detail`, `changes`, `command`, `cwd`, `reason`, `outsideSandbox`, `background`, `effect`, `allowForSession`, `agent` | `decision`, as Codex's: `"accept"`, `"acceptForSession"`, `"decline"`, `"cancel"` (no, and stop the turn; for a sub-agent's request, stop that sub-agent), or Koda's `{"declineWithFeedback": {"feedback": "..."}}`. `changes` is the edit's diff, as on the item. For a `Bash` or `Cargo` call, `command` and `cwd` say what would run and where, `reason` why the model wants it, and `outsideSandbox` that it asks to leave the sandbox, as fields a client can lay out; `detail` says the same in a line. `agent` names the sub-agent asking, when it isn't the main agent; a background agent may ask with no turn running, and `turnId` is then empty |
| `item/tool/requestUserInput` | `itemId`, `question`, `options` | `answer` |
| `turn/loopCap/requestDecision` | `cap`, `recentTools` | `decision`: `continue50`, `continue200` or `stop` |

In `koda exec` nothing reads an answer: Koda answers its own, as in
[Trust mode](#trust-mode-in-headless-mode), and prints `serverRequest/resolved`
with the request's id right after it. The answer shows in the call's item: a
refused call completes as `declined`.

### Usage

`turn/completed`'s `usage` counts this turn alone, however it ended, and
includes sub-agents, compaction, retries and the requests `WebSearch` sends
on a Claude model; `webSearchRequests` counts that model's searches, billed
apart from tokens. A model call cut off before the
provider reported usage counts for nothing, so a cancelled or failed turn
reports the calls that finished. `exec/summary`'s `usage` covers the whole
session, earlier turns of a resumed one included, and `usageByAgent` splits it
with the session's own calls under `main`: both are the last
`session/tokenUsage/updated`'s `byAgent`. Its `response` is the session's last
reply in this run, as `codex exec` takes its final agent message.

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

A new run without `--provider`, `--base-url` or `--model` starts from the
choice in `~/.config/koda/config.toml`, as `codex exec` starts from
`config.toml`; one flag alone keeps the rest of it (a different `--provider`
takes none of the saved model). Whichever provider runs, its endpoint comes
from `[model_providers.<id>] base_url` unless `--base-url` names one.

A resumed run keeps the session's own provider, endpoint and model, as
`codex exec resume` does: a model chosen by alias (`claude-opus`) moves to its
line's newest. `--provider`, `--base-url` or `--model` override them for this
run and from then on.

## Forking a session in headless mode

`koda exec fork <id> "<prompt>"` runs the prompt in a copy of the session, as
`codex exec fork` does: the copy has the whole history and the session's
provider and model (flags override them), and the original stays as it was.
The id may be the start of one; `-` reads the prompt from stdin. Without a
prompt Koda only forks and prints the copy's id (with `--json`,
`session/started`), so a script can continue it later with `koda exec
resume`:

```bash
fork=$(koda exec fork a3f8bc)
koda exec resume "$fork" "try the other approach"
```

With `--worktree`, the copy runs in a new worktree of the repository the
session worked in, wherever the command runs ([Sessions](./sessions.md#a-session-in-its-own-worktree)).
Two attempts from one conversation, each in its own checkout:

```bash
koda exec fork a3f8bc --worktree "implement option A" &
koda exec fork a3f8bc --worktree "implement option B" &
wait
```
