# The desktop app

Koda's desktop app is a window onto the same [background server](background-server.md)
as the TUI. A session is in both at once: start it in the app, go on in the
terminal with `koda resume`, or the other way round. The app follows Claude
Code's desktop app in what you see and do, and Codex in how it talks to the
server.

It runs on Apple Silicon Macs.

| Page | What it covers |
|---|---|
| This page | Installing and opening the app, updates, and what it doesn't do yet |
| [Your first session](./desktop-quickstart.md) | A walk through one task, from opening the app to reviewing the changes |
| [Sessions and the window](./desktop-sessions.md) | The session list, starting a session, the top bar, split view and archive |
| [Working in a session](./desktop-conversation.md) | The conversation, the prompt box, the model and mode, approvals, and the two panels |
| [Reviewing changes](./desktop-changes.md) | The Changes pane and commenting on lines |
| [Settings and shortcuts](./desktop-settings.md) | Theme, notifications and every keyboard shortcut |

## Install and open it

```bash
brew install koda
koda app
```

There's nothing to download or drag into Applications. `koda app` puts the
app in `~/Applications/Koda.app` (your own Applications folder, not the
system's `/Applications`) the first time, says so, and opens it. After that
you can open it from Launchpad, Spotlight or the Dock like any app.

After each `brew upgrade koda`, run `koda app` once more: it replaces the app
with the new version, so the app and `koda` are always one version.

The app is signed ad hoc, not with an Apple Developer ID. macOS opens it
because `koda app` installs it; a copy downloaded with a browser would be
quarantined and refused.

The app carries its own `koda`, and uses it to start the background server
when none is running, just as `koda` does. It reads your login shell's `PATH`,
so the commands Koda runs find the same tools as in your terminal. It never
reads your API keys or config: set those with `koda` (`/key`, `/provider`).

## Updates and the server

The app and the server must be one version. After `brew upgrade koda`:

- Run `koda app` again. It installs the new app and opens it.
- The old app, still open, says Koda was updated and offers to restart.
- If turns are running in the old server, the new app asks whether to wait
  for them or stop them before it replaces the server.

If the server goes away, the app reconnects on its own and starts a new
server if none answers. Closing the app stops nothing: a running turn goes
on in the server, and you can open it again in the app or the TUI. See
[The background server](background-server.md) for the whole picture.

## What it doesn't do yet

- Attach files or images to a prompt.
- Show what a command or a sub-agent changed in Changes.
- Archive from the TUI: use the app or `koda archive`.
- Run on Intel Macs, Linux or Windows.
