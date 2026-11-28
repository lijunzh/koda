# Koda as a server

`koda serve --stdio` serves Koda's [client protocol](headless.md#the-client-protocol)
to one program on its stdin and stdout, as `codex app-server` does; `koda serve`
alone serves [several clients on a socket](#the-shared-server). A GUI, an
editor or a script starts sessions, runs turns, steers and stops them, and
answers Koda's approvals and questions. The TUI and `koda exec` speak the same
protocol to a host in their own process.

```bash
koda serve --stdio
koda serve --stdio -c mode=plan --project-root ~/src/app
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
{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"sessionId":"a3f8bc12-…","input":[{"type":"text","text":"Why does the test fail?"}]}}
```

Koda answers each request by its `id`, and sends notifications as the turn runs:
`turn/started`, items that start, stream and complete, and one `turn/completed`
(see [Notifications](headless.md#notifications)). When it needs an answer it
sends a request of its own, such as `item/toolCall/requestApproval`, and the
turn waits:

```json
{"jsonrpc":"2.0","id":1,"method":"item/toolCall/requestApproval","params":{"sessionId":"a3f8bc12-…","itemId":"call_1","tool":"Write","…":"…"}}
{"jsonrpc":"2.0","id":1,"result":{"decision":"accept"}}
```

The second line is the client's answer, with the request's `id`; Koda numbers
its requests on its own, apart from the client's. Koda reads the answer
while the turn runs, then sends `serverRequest/resolved`. An answer to a
request that is already closed, or that Koda never sent, changes nothing.

`initialize` may also say what the client takes, as Codex's does, for that
connection only. `capabilities.optOutNotificationMethods` names notifications
it never wants, by exact method, such as a client that draws a reply only once
it completes and skips `item/agentMessage/delta`; requests and responses are
always sent. `capabilities.experimentalApi` opts into methods and
notifications still marked experimental: without it, such a method is
refused with `-32600` and "… requires experimentalApi capability", and such a
notification isn't sent. None is experimental yet. `clientInfo.title` is an
optional name to show people.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"my-app","title":"My App","version":"1.0"},"capabilities":{"experimentalApi":true,"optOutNotificationMethods":["item/agentMessage/delta"]}}}
```

`session/start` takes the model, provider and endpoint (`model`,
`modelProvider`, `baseUrl`), so `koda serve` doesn't take `--model`,
`--provider`, `--base-url` or `--resume`. `-c` and the other config flags set
what new sessions start from, as for `koda`. An earlier session is opened
with `session/resume` and its `sessionId`, as Codex's `thread/resume`; it
takes the same fields, and keeps its own provider. Without a `cwd` it opens in
the folder it was started in, as Codex's does.

`session/start` also takes the session's folder and its own config, as
Codex's `thread/start` does: `cwd`, an absolute path to an existing folder
(the server's own when left out), and `config`, a map of dotted keys to
values, as `-c` sets them for that session only (`{"mode": "plan"}`). A key
Koda doesn't know, or a folder that isn't one, is refused. So one shared
server serves every project. `session/fork` takes both for the copy, which
belongs to its `cwd`.

## What a prompt is

`turn/start` takes `input`, the prompt as a list of parts in the order the user
composed it, as Codex's `input` and the Agent Client Protocol's prompt are:

```json
{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{"sessionId":"a3f8bc12-…","input":[
  {"type":"text","text":"Compare @src/lib.rs with this screenshot"},
  {"type":"mention","name":"lib.rs","path":"src/lib.rs"},
  {"type":"localImage","path":"/tmp/shot.png"},
  {"type":"skill","name":"review-checklist","path":"/home/me/.koda/skills/review-checklist/SKILL.md"}
]}}
```

The parts are Codex's `UserInput`, with its names:

- **`text`**, with optional `text_elements` (spans the client drew as something
  else, each `byteRange` and `placeholder`), kept for the client and not sent
  to the model.
- **`image`**, a `url`: a `data:` URL with base64 content. Codex also takes an
  `https` URL or an uploaded file; Koda doesn't.
- **`localImage`**, a `path` on the server's machine, read under the `Read`
  tool's rules.
- **`skill`**, a `name` and `path` as `skills/list` gives them (see
  [Named skills](#named-skills)).
- **`mention`**, a `name` to show and a `path`: a file or folder the user
  named with `@`, or `server:uri` for an MCP resource. Koda reads it on its own
  machine, under the `Read` tool's rules, and sends the model its contents (see
  [File & image attachments](./attachments.md#mentioning-files)), as Claude
  Code includes an `@` file. This is Koda's meaning: Codex's `mention` names an
  app, plugin or thread, and Codex leaves a file as a path in the text.

The model is sent one message, as before the list: the text parts in order, a
blank line between, with each mention's contents and each skill's instructions
after them, and the images, which go before the text for Claude (as Anthropic
recommends) and after it for an OpenAI-compatible model. Images aren't placed
between text parts. The `userMessage` item keeps the parts as sent, in
`content`. `turn/steer` takes
the same list, but only `text` parts: steering reaches the model as text.

## Finding files to mention

`fuzzyFileSearch`, Codex's method, finds the files and folders a client offers
for an `@` mention, so the client never walks the project itself:

```json
{"jsonrpc":"2.0","id":7,"method":"fuzzyFileSearch","params":{"query":"auth","roots":["/home/me/app"],"cancellationToken":"composer"}}
{"jsonrpc":"2.0","id":7,"result":{"files":[{"root":"/home/me/app","path":"src/auth.rs","match_type":"file","file_name":"auth.rs","score":65645,"indices":[4,5,6,7]}]}}
```

Only the `roots` are searched, and they must be absolute paths. Hidden files
are included and links followed; inside a git repository, what `.gitignore`
ignores is left out. A file whose own name matches ranks above one that matches
only by its folders. At most 50 matches come back, best first, equal scores by
path, with `match_type` `file` or `directory` and `indices`, the matched
characters in `path`, for highlighting. The result fields keep Codex's
snake_case names. An empty `query` or no `roots` finds nothing. A request with
the same `cancellationToken` as one still running cancels it, and the earlier
one answers with no files: send one token per text box, a request per
keystroke. Koda's TUI fills its `@` list this way.

## Named skills

A `$name` in `input` that names a skill loads it, as Codex reads `$` mentions:
Koda sends the model the skill's `SKILL.md` after the text, in Codex's
`<skill>` block (`<name>`, `<path>`, then the instructions). A `skill` part
does the same for a skill picked without `$name` in the text; its `name`
decides. A model-only skill isn't loaded, and a
`$name` that names no skill, or a common shell variable such as `$PATH`, stays
text.

## Listing sessions

`session/list` gives the stored sessions, as Codex's `thread/list` does:

```json
{"jsonrpc":"2.0","id":6,"method":"session/list","params":{"cwd":"/Users/me/project","sortKey":"updated_at","limit":20}}
```

- **`cwd`** is one folder or a list of them; without it, every folder's
  sessions. Name a folder as its sessions report it (`cwd`): links aren't
  followed.
- **`sourceKinds`** picks how the sessions came to be: `cli` (the terminal
  UI), `exec` (`koda exec`), `appServer` (another client of the server),
  `subAgent` (a session's sub-agent) and `unknown` (from before this was
  recorded). Without it, the ones a person started: `cli`, `appServer` and
  `unknown`. A session started with `ephemeral`, such as the TUI's side
  conversations and reviews, is never listed.
- **`modelProviders`** keeps the sessions of those providers.
- **`archived`**, when `true`, gives only archived sessions. Without it, or
  with `false`, only the ones that aren't, as Codex's `thread/list`.
- **`sortKey`** is `created_at` (the default) or `updated_at`, and
  **`sortDirection`** `desc` (the default, newest first) or `asc`.
- **`limit`** is 50 without one. Pass the answer's `nextCursor` as `cursor`,
  with the same filters and order, for the next page; it is absent on the
  last. A cursor names the page's last session, so a session made meanwhile
  doesn't shift the next page.

Each session has its `id`, `name`, `preview` (the start of its first prompt),
`createdAt` and `updatedAt` in Unix seconds, `cwd`, the `model` and `modelProvider`
it last ran with, `mode`, where it came from (`source`, `originator`, the
name its client gave in `initialize`, `forkedFromId` and `parentSessionId`),
and its `status`, as Codex's `Thread.status`: `{"type": "notLoaded"}` for a
stored session that isn't open, `{"type": "idle"}`, or `{"type": "active",
"activeFlags": […]}` while a turn runs or an answer is waited on, with
`waitingOnApproval` (a tool call's approval, the loop cap, an MCP server's
request) and `waitingOnUserInput` (the model's question) among the flags.

Every client hears `session/started`, `session/name/updated`,
`session/deleted`, `session/archived`, `session/unarchived` and
`session/status/changed` for every session, as Codex
sends them to every connection, so a list a client shows stays right when
another client makes, renames, archives or deletes a session, and shows what
each is doing. `koda exec --json` leaves `session/status/changed` out. A session's turns and requests go only to the clients that
have it open.

## Archiving a session

`session/archive` puts a stored session away and `session/unarchive` brings
it back, as Codex's `thread/archive` and `thread/unarchive` do:

```json
{"jsonrpc":"2.0","id":7,"method":"session/archive","params":{"sessionId":"<id>"}}
{"jsonrpc":"2.0","id":7,"result":{}}
{"jsonrpc":"2.0","method":"session/archived","params":{"sessionId":"<id>"}}

{"jsonrpc":"2.0","id":8,"method":"session/unarchive","params":{"sessionId":"<id>"}}
{"jsonrpc":"2.0","id":8,"result":{"session":{"id":"<id>","status":{"type":"notLoaded"}}}}
{"jsonrpc":"2.0","method":"session/unarchived","params":{"sessionId":"<id>"}}
```

- **An archived session** is left out of `session/list` unless `archived` is
  `true`. `session/read`, `session/turns/list` and `session/fork` still work
  on it (the fork isn't archived). `session/resume` is refused with
  `-32600`: "session ID is archived. Run `koda unarchive ID` to unarchive it
  first.", with the session's ID.
- **An open session is closed first,** whatever it is doing, in every
  client: its running turn stops (`turn/completed`, `interrupted`), the
  requests waiting in it are withdrawn (`serverRequest/resolved`), and it
  becomes `notLoaded` (`session/status/changed`). No client stays
  subscribed to it. Then `session/archived` follows.
- **Sub-agents' sessions** (the ones whose `parentSessionId` leads to it)
  are archived and unarchived with it. Every client hears one
  `session/archived` or `session/unarchived` for each session changed, the
  session's own first. Forks aren't touched.
- **`session/unarchive`** answers with the session as `session/read` gives
  it, and doesn't open it: `session/resume` does.
- **Refused with `-32600`:** archiving an archived session ("session ID is
  archived"), unarchiving one that isn't ("session ID is not archived"), an
  ID that names no session, and a session another process is writing.

The `Session` object has no archived field, as Codex's `Thread` has none: a
client knows from the list it asked for and from the two notifications.

## A session's earlier turns

`session/resume`, Codex's `thread/resume`, reopens a stored session and returns all its turns,
and `session/read` with `includeTurns` reads them without opening it. A client
that shows only recent turns, such as a phone or a long session, pages them
with `session/turns/list`, as Codex's `thread/turns/list` does:

```json
{"jsonrpc":"2.0","id":7,"method":"session/turns/list","params":{"sessionId":"a3f8bc12-…","limit":10}}
```

The answer's `data` has up to `limit` turns (25 without one, at most 100),
newest first, each with its items. An item is flat, as Codex's `ThreadItem`:
the item's own fields (`type`, `id`, …) with `startedAtMs` and `completedAtMs`
beside them, and `parentItemId` on a sub-agent's. A turn still running is `inProgress`, with
the items it has so far as its clients were told of them: those that
completed, and those still open, a message with its text so far and a tool
call still running, as a client joining the session gets it and as Codex's
`thread/read` merges a thread's active turn. `session/read` gives the same. Pass `nextCursor` as `cursor` for the next older page;
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
| An experimental method without `experimentalApi` | `-32600`, `… requires experimentalApi capability` |
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
- **Attaching.** `session/resume` with the id of a session that's open
  attaches the client to it, as Codex's `thread/resume` subscribes to a loaded
  thread: the answer is the session as it runs, with its turns, and nothing
  starts again. A turn that is running comes as it stands, `inProgress`, with
  its completed items and then the ones still open: a message with its text
  so far, a tool call `inProgress` with the notices sent about it (an open
  item has no `completedAtMs`). The answer's `settings` say what the
  session's turns run with (model and alias, provider and endpoint, effort,
  mode), as `session/settings/updated` does. After it come what the client
  wasn't there to hear: the token counts, the plan and the turn's diff as
  last sent, then the requests still waiting. The client hears nothing of
  the session before the answer, and after it everything that follows,
  once: the session answers at one point in its own stream of messages,
  and the client is subscribed there.
- **One writer per session.** One process writes a session at a time, as
  Codex keeps one writer per thread. While a session is open in one Koda
  process (this server, a `koda exec` run, or a TUI started with
  `--no-daemon` or `--oss`), opening, renaming or deleting it from another
  is refused with `-32600` and "session … already has an active writer";
  `session/read` and `session/turns/list` still read it. The lock files live
  in `session-writer-locks/` beside Koda's database, and a process that ends
  lets its sessions go.
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

## One version, and updates

Every Koda client and the server run the same version: a client uses only a
server whose `serverInfo.version` in the `initialize` answer is its own. Koda
doesn't keep its protocol compatible across versions, so a client of another
app (the desktop app included) follows the same rule:

- **An older server is replaced.** Send `serve/stop` with `forUpdate` (your
  version) and `ifIdle`. Connected clients don't hold it back, since none can
  go on with a server of another version: with no turn running and no request
  waiting it answers `{"stopped": true}`, tells every client, and stops; start
  a server of your version once its socket is gone. Otherwise it answers
  `{"stopped": false, "working": N}` and changes nothing: ask the user, as
  Codex's TUI does before a restart that may interrupt work, then send
  `serve/stop` with `forUpdate` and `afterTurns` to wait for those turns, or
  with `forUpdate` alone to stop them now (they end `interrupted`, stored).
  While it waits it answers `{"draining": true}` and refuses new turns
  (`turn/start`, `session/compact/start`, `session/goal/set`) with Codex's
  error, `"Server is draining; retry after reconnecting"`,
  `data: {"reason": "serverShuttingDown"}`, and stops after the last one.
- **A newer server means Koda was updated** while you were running. Don't use
  it, and don't stop or replace it; say the client must be started again.
- **`serve/updating`** goes to every client before the server stops for an
  update: `version` is the one replacing it, and `working` the sessions it
  still waits for (0 when it stops at once). A client of the old version
  can't go on afterwards, so it tells its user to start it again rather than
  reconnecting, and doesn't start a server of its own version in the gap.

Codex drains its app-server in the same way when its manager restarts it onto
a new version (60 seconds for running turns, then it stops them); its clients
use a server of another version and warn. Koda waits for the user's choice
and doesn't use a server of another version.

### `koda serve --start`

A client doesn't have to do all of the above itself: `koda serve --start`
does it, as `koda` does at start-up, and as Codex's apps run
`codex app-server daemon start`. Run the `koda` of your client's version,
with the environment the server should have (it keeps the environment of
whatever starts it). It makes sure a server of its version answers at Koda's
socket (or the one `--listen` names), then prints one JSON object and exits:

| `status` | Exit | Meaning |
|---|---|---|
| `alreadyRunning` | 0 | A server of this version was running |
| `started` | 0 | None answered, so one was started; it stops after ten minutes with no client and no work |
| `restarted` | 0 | An older one was replaced |
| `busy` | 1 | An older one has turns running in `working` sessions, and nothing changed. Ask the user, then run it again with `--running-turns wait` or `--running-turns stop`. `draining: true` means it already waits for them, so only `wait` is left |
| `newerServer` | 1 | Koda was updated since this `koda` was installed: start the updated one |
| `predatesUpdate` | 1 | A server from before 0.7.13 with windows open: `koda serve --stop`, then start again |
| `failed` | 1 | It couldn't be reached or started |

Every answer has `socketPath` and `cliVersion`, `serverVersion` when a server
answered, and `message` to show a person when it isn't ready. With
`--running-turns wait` it returns once those turns have finished and the new
server is up, which can take as long as they do. A server it starts has 30
seconds to answer, as Codex's daemon has; one that exits without answering
is `failed` a few seconds after, and `message` says it exited.

```console
$ koda serve --start
{"cliVersion":"0.8.1","serverVersion":"0.8.1","socketPath":"/Users/me/.config/koda/serve/koda.sock","status":"started"}
```

No client owns the server: closing one stops nothing, and the last one to go
leaves it to its idle exit.

**What stays the same across versions** is only what two versions need to
speak to each other for this: `initialize` and the `serverInfo.version` in
its answer, `serve/stop` with its fields and answer, and `serve/updating`.
Anything else in the protocol may change in any release, with the client
and server changing together.

## The TUI on the shared server

How to think about the server, and the commands to manage it, are in
[The background server](./background-server.md); this section is the detail.

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
- **If the server goes away** (it crashed or was stopped while the TUI was
  open), the TUI reconnects, as Codex's TUI does: it tries at once, then after
  1, 2 and 4 seconds, then every 8 seconds, for two minutes, starting Koda's
  own server again when nothing answers (not one given with `--remote`). The
  session opens again where it was, and an approval it was waiting for comes
  back. A turn the server kept running is followed again; a turn that was
  running when the server itself stopped ended with it, and the TUI says so.
  A side conversation or `/review` ends with the connection: the TUI returns
  to the main session. Ctrl+C quits while it tries. If it gives up, your next
  prompt tries again.
- **After an update,** the first `koda` you run replaces the older server
  (see [One version, and updates](#one-version-and-updates)). Windows that are
  only open don't hold it back. If turns are running in other windows, it
  asks: wait for them to finish (the default, and what it does with no
  terminal to ask on; Ctrl+C leaves, and the server restarts after them
  anyway), restart now (those turns stop, saved as interrupted), or quit and
  leave it running. The older windows say Koda was updated; once their server
  has gone they ask you to quit and run `koda --resume <id>` rather than
  reconnecting. A `koda` older than the running server doesn't use or replace
  it, and says Koda was updated. A server from before 0.7.13 doesn't know
  this and refuses while other windows are open: `koda` then says to run
  `koda serve --stop` (which stops their turns) and start it again.
- **API keys:** the server has the environment of the shell that started it.
  A key saved with `/key` works for every TUI; one only in a shell's
  environment reaches the server only if that shell started it.
