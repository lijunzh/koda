# ACP (removed)

Koda no longer has an ACP server. `koda server` has been removed, so editors
that speak the [Agent Client Protocol](https://agentclientprotocol.com) can't
connect to Koda. The `koda connect` placeholder, which never did anything, is
gone too.

If you registered Koda as an agent in your editor, remove that entry. Running
`koda server` now exits with an error that points to this page.

## Why it went

It didn't work. The server stopped reading input while a turn ran, so the first
approval in Safe mode deadlocked. It also ignored the workspace and session a
client asked for, and treated "always allow" as a rejection.

Fixing that meant building a second protocol for a client nobody was using.
Koda now has one protocol instead of two half-built ones.

## What to use instead

| You want to | Use |
|-------------|-----|
| Work with Koda from an editor | Run `koda` in the editor's terminal |
| Call Koda from a script | `koda -p "prompt"` ([headless mode](headless.md)) |
| Read a run's events from a program | `koda -p "prompt" --output-format stream-json` ([wire protocol](headless.md#wire-protocol)) |

`stream-json` is output only. A run takes one prompt and applies the
[headless policy](headless.md#trust-mode-in-headless-mode) to approvals, so
another program can follow a run but can't steer it or answer its approvals.
There's no way to do that yet.
