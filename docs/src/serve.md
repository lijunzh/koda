# Koda as a server

`koda serve --stdio` serves Koda's [client protocol](headless.md#the-client-protocol)
to one program on its stdin and stdout, as `codex app-server` does. A GUI, an
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

## Not yet

- **One client per process.** A shared server that several clients attach to,
  on a Unix socket, comes later.
- **`@file` mentions** are read by the client today; with a server on another
  machine, the server will read them.
