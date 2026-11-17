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

Codex drops a line it can't parse, with no answer. Koda answers it, as JSON-RPC
says to, since it keeps `"jsonrpc"` so that standard libraries can be used.

## When the client reads slowly

Messages wait in a queue until the client reads them, in order; none is
dropped, and Koda's turns don't wait for the client. Codex's stdio does the
same. The queue grows only while the client lags, so read stdout promptly, on
its own thread or task if your program blocks elsewhere.

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
  crashed server is replaced.
- **It runs until** Ctrl+C or `SIGTERM`, then stops every session and removes
  the socket.

## Not yet

- **The TUI on the shared server.** Next, the TUI starts `koda serve` when
  none is running and works through it, as Codex's TUI does; `--no-daemon`
  keeps it in its own process, and `koda --remote` attaches elsewhere.
