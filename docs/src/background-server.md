# The background server

Your sessions don't run in the terminal window you type in. They run in
**one background process per user, `koda serve`**. The TUI is a window onto
it, and so are a second TUI and other programs such as the desktop app. This
is how Codex's TUI uses its background server and how Docker's command line
uses its daemon.

```text
 koda (a TUI)  ─┐
 koda (another) ─┼── ~/.config/koda/serve/koda.sock ──▶ koda serve ──▶ the model's API
 other clients ─┘
```

`koda serve` holds what is happening: the open sessions, their running turns,
the approvals waiting for an answer, and the MCP servers. It stores every
session in Koda's database (`~/.config/koda/db/`), so a session outlives the
window, the server and a restart of the Mac.

## You rarely need to manage it

- **It starts on its own.** The first `koda` that finds no server starts one
  in the background. Every later `koda` joins it.
- **No window owns it.** Closing a window, or the terminal it ran in, stops
  nothing: a turn that was running goes on, and an approval it was waiting
  for waits for whoever opens the session next (`koda resume`).
- **It stops on its own** once it has had no window and no work for ten
  minutes.
- **It follows upgrades.** After `brew upgrade koda`, the next `koda` you run
  replaces the older server: at once if nothing runs in it, otherwise after
  asking whether to wait for the running turns or stop them. See
  [One version, and updates](./serve.md#one-version-and-updates).

## Sessions on the server

A session is in one of three states, which every window can see:

| State | Meaning |
|---|---|
| Stored (`notLoaded`) | Only in the database. Opening it loads it |
| Open, idle (`idle`) | Loaded in the server, nothing running |
| Working (`active`) | A turn is running, or it waits for an approval or an answer |

- **Opening** a session (`koda resume`, `/resume`, or a click in another
  client) loads it if it is stored, or joins it if it is already open,
  showing everything so far, a turn halfway through included.
- **A session closes** a minute after the last window leaves it, unless a turn
  is still running. A turn runs to its end with no window open.
- **An approval nobody answers** is refused after a day
  (`approval_wait_timeout_secs`).

**The same session in two windows** (two TUIs, or the TUI and the app):
both show everything as it happens.

- **Approvals:** each one appears in both. The first answer counts, and the
  other window's prompt goes away.
- **Turns:** one turn runs at a time. A new turn sent from the other window
  while one runs is refused.
- **Settings:** changing the model or the mode in one window changes it for
  the session, so the other window shows it too.
- **Runs outside the server:** `koda exec`, `koda --no-daemon` and
  `koda --oss` each run in their own process. Only one process may write a
  session at a time, so one of these can't open a session the server has
  open, and the server can't open one they have.

## Commands

| To | Run |
|---|---|
| Start it, or check that it runs your version | `koda serve --start` |
| Stop it | `koda serve --stop` |
| Run it in this terminal, to watch it | `koda serve` (Ctrl+C stops it) |
| See whether one is running | `pgrep -fl "koda serve"` |
| Read what it logged | `koda-logs --follow` |
| Work without it | `koda --no-daemon` |

**`koda serve --start`** starts a server if none answers, or replaces an older
one, then prints one line of JSON and exits. `status` says what it found:
`alreadyRunning`, `started` or `restarted`. When an older server still has
turns running it changes nothing and says `busy`: run it again with
`--running-turns wait` or `--running-turns stop`. The full list is in
[`koda serve --start`](./serve.md#koda-serve---start).

**`koda serve --stop`** stops every running turn (each is saved as
interrupted) and closes every window's connection. Sessions stay stored and
open again as they were. A `koda` window still open reconnects and starts a
new server, so quit the windows first if you want it to stay stopped.

**`koda serve`** in a terminal runs the server there, with no idle exit,
until Ctrl+C. Only one server can own the socket, so stop the background one
first.

**`koda-logs`** reads the log database the server writes to:
`--follow` keeps printing new lines, `--level warn` shows only problems, and
`--session <id> --sessionless` shows one session's lines with the server's
own. See [Debugging a run](./debugging.md).

**There is no status command yet.** `pgrep` shows whether a server process
runs; it also lists servers that tests or another `--listen` socket started,
each with its socket in its command line. `koda serve --start` reports the
version of the one at your socket, but starts one if none answers.

## Things to know

- **API keys:** the server has the environment of whatever started it. A key
  saved with `/key` works everywhere; a key that exists only in one shell's
  environment reaches the server only if that shell started it. To give a
  running server a new environment, `koda serve --stop`, then run `koda` from
  that shell.
- **Config changes:** `-c` values, and flags that set one (`--mode`,
  `--reasoning-effort`, `--agent`, …), can't change the running server's
  config, so a `koda` given one runs in its own process instead. Changes saved
  to `config.toml` (`/model`, `/provider`, `koda mcp add`) reach new
  sessions on the server; open sessions pick up MCP changes after
  `/mcp reconnect`.
- **If the server dies** while a window is open, the window reconnects for
  two minutes, starting a new server when none answers, and reopens its
  session. A turn that was running when the server died is saved as
  interrupted.
- **Another socket:** `koda serve --listen unix://PATH` runs a server
  elsewhere, and `koda --remote unix://PATH` uses it. Koda doesn't start or
  replace a server it was pointed at.
- **Privacy:** the socket is readable by you only, and commands Koda runs in
  its sandbox can't connect to it.

For programs that talk to the server directly, the protocol is in
[Koda as a server](./serve.md).
