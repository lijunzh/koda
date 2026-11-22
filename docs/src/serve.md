# Koda as a server

`koda serve --stdio` serves Koda's [client protocol](headless.md#the-client-protocol)
to one program on its stdin and stdout, as `codex app-server` does; `koda serve`
alone serves [several clients on a socket](#the-shared-server). A GUI, an
editor or a script starts sessions, runs turns, steers and stops them, and
answers Koda's approvals and questions. The TUI and `koda exec` speak the same
protocol to a host in their own process.

```bash
koda serve --stdio
koda serve --stdio -c mode=safe --project-root ~/src/app
```

Each message is one line of JSON-RPC 2.0, with `"jsonrpc": "2.0"`, each way.
[`protocol-schema.json`](protocol-schema.json) has every request, response and
notification.

## A session, line by line

The client sends `initialize` first. Koda answers it before reading the next
line, so requests may follow straight behind it:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"my-app","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"session/start","params":{"model":"claude-haiku"}}
{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"sessionId":"a3f8bc12-…","input":"Why does the test fail?"}}
```

Koda answers each request by its `id`, and sends notifications as the turn runs:
`turn/started`, items that start, stream and complete, and one `turn/completed`
(see [Notifications](headless.md#notifications)). When it needs an answer it
sends a request of its own, such as `item/toolCall/requestApproval`, and the
turn waits:

```json
{"jsonrpc":"2.0","id":1,"method":"item/toolCall/requestApproval","params":{"sessionId":"a3f8bc12-…","itemId":"call_1","tool":"Write","…":"…"}}
{"jsonrpc":"2.0","id":1,"result":{"decision":{"type":"approve"}}}
```

The second line is the client's answer, with the request's `id`; Koda numbers
its requests on its own, apart from the client's. Koda reads the answer
while the turn runs, then sends `serverRequest/resolved`. An answer to a
request that is already closed, or that Koda never sent, changes nothing.

`session/start` takes the model, provider and endpoint (`model`,
`modelProvider`, `baseUrl`), so `koda serve` doesn't take `--model`,
`--provider`, `--base-url` or `--resume`. `-c` and the other config flags set
what new sessions start from, as for `koda`.

`session/start` also takes the session's folder and its own config, as
Codex's `thread/start` does: `cwd`, an absolute path to an existing folder
(the server's own when left out), and `config`, a map of dotted keys to
values, as `-c` sets them for that session only (`{"mode": "plan"}`). A key
Koda doesn't know, or a folder that isn't one, is refused. So one shared
server serves every project. `session/fork` takes both for the copy, which
belongs to its `cwd`.

## Mentioned files

`turn/start` takes `mentions: [{"path": "src/lib.rs"}]` beside `input`, for the
files and directories the user mentioned with `@`. Koda reads them on its own
machine, under the `Read` tool's rules, and sends the model their contents after
the text (see [File & image attachments](./attachments.md#mentioning-files)). The
`userMessage` item keeps the text as the user wrote it and lists the paths under
`mentions`.

## Named skills

A `$name` in `input` that names a skill loads it, as Codex reads `$` mentions:
Koda sends the model the skill's `SKILL.md` after the text, in Codex's
`<skill>` block (`<name>`, `<path>`, then the instructions). `turn/start` also
takes `skills: [{"name": "review-checklist"}]`, as Codex's `UserInput::Skill`,
for a skill picked without `$name` in the text; `path`, as `skills/list` gives
it, is optional, since the name decides. A model-only skill isn't loaded, and a
`$name` that names no skill, or a common shell variable such as `$PATH`, stays
text.

## A session's earlier turns

`session/start` with a stored session's id reopens it and returns all its turns,
and `session/read` with `includeTurns` reads them without opening it. A client
that shows only recent turns, such as a phone or a long session, pages them
with `session/turns/list`, as Codex's `thread/turns/list` does:

```json
{"jsonrpc":"2.0","id":7,"method":"session/turns/list","params":{"sessionId":"a3f8bc12-…","limit":10}}
```

The answer's `data` has up to `limit` turns (25 without one, at most 100),
newest first, each with its items; a turn still running is `inProgress`, with
the items it has so far. Pass `nextCursor` as `cursor` for the next older page;
it is absent on the last. `"sortDirection": "asc"` pages from the oldest
instead. `backwardsCursor`, with the other direction, reads back from a page's
first turn, that turn included, to catch what changed in a turn that was
running. Cursors are opaque, and one whose turn is gone is refused.

## A session's goal

`session/goal/set` gives an open session a goal Koda keeps working on, as
Codex's `thread/goal/set` does; `session/goal/get` reads it and
`session/goal/clear` removes it:

```json
{"jsonrpc":"2.0","id":8,"method":"session/goal/set","params":{"sessionId":"a3f8bc12-…","objective":"Get the test suite passing","tokenBudget":500000}}
```

With no goal, an `objective` starts one, `active` unless `status` says
otherwise. With one, the fields given change it: `status` (`active` resumes,
`paused` stops), `tokenBudget` (`null` removes the limit) or `objective`, which
the running turn is told before its next step. While the goal is `active` and
the session is idle, Koda starts a turn on it by itself: `turn/started` with no
`turn/start` before it, and no `userMessage` item. `session/goal/updated`
reports every change, by a client, by the model's `CreateGoal` or `UpdateGoal`
(with the `turnId`), or by Koda as a turn ends: its `tokensUsed` and
`timeUsedSeconds`, or a new status (`complete`, `blocked`, `budgetLimited`).
`session/goal/cleared` follows a clear.

With `goal_judge = "evaluator"`, a check after each turn on an active goal
decides, and `session/goal/judged` reports what it found, with the `turnId`:
`verdict` `met` (the goal is then `complete`), `notYet` (the next turn takes the
`reason` as guidance) or `impossible` (the goal is then `blocked`):

```json
{"jsonrpc":"2.0","method":"session/goal/judged","params":{"sessionId":"a3f8bc12-…","turnId":"7c1e…","verdict":"notYet","reason":"The tests haven't been run."}}
```

After eight of Koda's goal turns in a row that call no tool, Koda holds the
goal, as Claude Code stops such a loop: `session/goal/held` (Koda's own) says
so, with the `turnId` and `turns`, and the goal, still active, goes on as after
an interrupt (below).

```json
{"jsonrpc":"2.0","method":"session/goal/held","params":{"sessionId":"a3f8bc12-…","turnId":"7c1e…","turns":8}}
```

An interrupt ends the turn, not the goal, but no turn starts on the goal by
itself after it, as in Claude Code: the goal goes on once the next `turn/start`
turn ends, or at once after a `session/goal/set` (such as `status: "active"`).
`turn/start` during one of Koda's own turns is refused as during any turn: send
`turn/steer` instead.

## Background commands

A command the model starts with `Bash` `background: true` outlives its turn;
`session/backgroundTerminals/list`, `terminate` and `clean` list and stop them.
When one exits by itself, Koda tells the model, as Claude Code does: its
command, exit code, run time and last lines, at the running turn's next step,
or in a turn Koda starts for it when the session is idle (`turn/started` with
no `turn/start` before it). `session/backgroundTerminals/exited` reports it,
with the `turnId` of the turn that tells the model:

```json
{"jsonrpc":"2.0","method":"session/backgroundTerminals/exited","params":{"sessionId":"a3f8bc12-…","turnId":"7c1e…","data":[{"processId":"5821","command":"cargo test","exitCode":0}]}}
```

A process a client or Koda stopped isn't reported. While one the model started
is running, Koda starts no turn on the session's goal by itself; its exit
does, and a check-in follows if the goal waits past `goal_checkin_minutes`.

`list`'s `waiting` says whether Koda will start a turn by itself for the
model's background commands: one is running, or has ended and the model hasn't
been told yet. Processes an ordinary command left running are listed but don't
count. `koda exec` waits while it's true, as `claude -p` waits for background
work. In a session started with `approvalPolicy: never`, which nobody attends,
background commands have a time limit, as Claude Code's do in an unattended
session: `background_time_limit_minutes` (10 by default), or the `timeout`
the model passes, up to 2 hours. Koda stops a command at its limit and tells
the model, which can start it again with a longer `timeout`.

## Errors

| The client sends | Koda answers |
|------------------|--------------|
| A line that isn't JSON, or isn't UTF-8 | `-32700`, with `"id": null` |
| JSON that isn't a JSON-RPC message, or a batch (an array) | `-32600`, with the message's `id` when it has one |
| A request before `initialize`, or a second `initialize` | `-32600`, `Not initialized` or `Already initialized` |
| A method Koda doesn't have | `-32601` |
| Parameters that don't fit | `-32602` |
| A request about a session that isn't open, or a turn that isn't running | an error that says so; nothing else is affected |
| A notification | nothing: Koda defines none from the client |
| A message over 64 MiB | `-32600`, with `"id": null`, and the connection ends |

Codex drops a line it can't parse, with no answer. Koda answers it, as JSON-RPC
says to, since it keeps `"jsonrpc"` so that standard libraries can be used.

64 MiB is the limit Codex's WebSocket transport has (tungstenite's default): a
pasted image in base64 fits, and a client that never sends a newline can't make
Koda read without end.

## When the client reads slowly

Messages wait in a queue until the client reads them, in order; none is
dropped, and Koda's turns don't wait for the client. Codex's stdio does the
same. The queue grows only while the client lags, so read stdout promptly, on
its own thread or task if your program blocks elsewhere.

On the shared server's socket (below), a client is let go instead once 4,096
messages wait for it, as Codex lets a socket client go when its queue fills:
what waited is dropped and the connection closes. Its sessions go on; attaching
again reads them and resends any request still waiting.

## Ending

Koda stops when stdin closes, when stdout breaks, or on Ctrl+C or `SIGTERM`
(the client may keep stdin open). Running turns stop, each with one
`turn/completed` (`interrupted`); open requests are closed with
`serverRequest/resolved`; what is queued is written while the pipe takes it;
and the process exits 0.

## The shared server

`koda serve` with no transport flag is the shared server: one Koda that
several clients use at once, on a Unix socket, as
`codex app-server --listen unix://` serves them.

```bash
koda serve                                   # ~/.config/koda/serve/koda.sock
koda serve --listen unix:///tmp/koda/k.sock  # somewhere else
```

Each connection speaks what `--stdio` speaks, one JSON-RPC line each way,
`initialize` first. (Codex frames its socket as WebSocket; Koda's clients are
local programs, so one framing for both transports keeps their code the same,
and `nc -U ~/.config/koda/serve/koda.sock` is enough to try it.)

- **Each client hears its own sessions.** A connection is subscribed to the
  sessions it starts or forks; a session's notifications and requests go to
  its subscribers, and a message that belongs to no session goes to every
  connection.
- **A session outlives its client.** A connection that closes, or sends
  `session/unsubscribe`, leaves its sessions open, and a running turn goes on.
  A session with no client and no running turn for a minute is closed, as
  Codex unloads an idle thread.
- **Attaching.** `session/start` with the id of a session that's open
  attaches the client to it, as Codex's `thread/resume` subscribes to a loaded
  thread: the answer is the session as it runs, with its turns, and nothing
  starts again.
- **Requests wait for a client.** An approval, a question or the loop cap goes
  to every client attached to its session; the first answer counts, and the
  others get `serverRequest/resolved`, so they can drop their prompt. A client
  that attaches later is sent the requests still waiting, after the session.
  One that nobody answers within a day (`approval_wait_timeout_secs`) is
  answered no: the approval is refused, the question gets an empty answer and
  the loop stops.
- **Answers** come back on the connection that asked. Koda takes an answer to
  one of its own requests (an approval, a question) from any connection; the
  first one counts.
- **Private to you.** The socket is readable by its owner only, in a folder
  only you can change; a connection from another user is refused, and a lock
  keeps a second `koda serve` from taking the socket over. A socket left by a
  crashed server is replaced. Sandboxed commands can't connect to it.
- **A client that stops reading is let go** once 4,096 messages wait for it
  (a TUI suspended with Ctrl+Z, a client that hangs), as Codex does, so one
  stuck client can't make the server hold everything.
- **After a crash,** the next server opens a session from what was stored. A
  turn that was running when the server died reads as `interrupted`, and its
  question stays in the conversation.
- **It runs until** Ctrl+C, `SIGTERM` or `koda serve --stop`, then stops
  every session and removes the socket. Its sessions stay stored, and open
  again as they were. Over the protocol, `serve/stop` does the same; with
  `ifIdle` it stops only if no session is in use (a client, a running turn or a
  request waiting) and no other client is connected.

## The TUI on the shared server

`koda` works on the shared server, as Codex's TUI works on its daemon: it
connects to `~/.config/koda/serve/koda.sock`, and when nothing answers it
starts `koda serve` in the background, which stops once it has had no client
and no work for ten minutes. Your sessions run there, so a TUI you close
leaves the session for the next one: `koda resume` picks it up, and an approval
it was waiting for is still there.

- **Its own process instead** with `--no-daemon`, and for runs the shared
  server can't serve as asked, as Codex's TUI does: `--oss` (its model server
  and key live in that process) and any `-c` value or flag that sets one
  (`--mode`, `--reasoning-effort`, `--agent`, …), since the server reads its
  own config. `koda exec` always runs in its own process.
- **Another server:** `koda --remote unix://PATH` uses the server at that
  socket; it isn't started or replaced.
- **After an update,** a server of another version is replaced if nothing is
  in use, as Claude Code's supervisor restarts onto a new version; if it's
  busy, this run uses its own process and says so. `koda serve --stop` stops
  it.
- **API keys:** the server has the environment of the shell that started it.
  A key saved with `/key` works for every TUI; one only in a shell's
  environment reaches the server only if that shell started it.
