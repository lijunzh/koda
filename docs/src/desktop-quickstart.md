# Your first session in the app

One task from start to finish. It takes a few minutes and assumes
[Koda is installed](./installation.md) with Homebrew on an Apple Silicon Mac.

## 1. Set up a provider, once, in the terminal

The app uses the provider, key and default model you set up for `koda`. It
has no screen of its own for them. If you've never run Koda:

```bash
koda
```

Its first run asks for a provider and a key, or points you at a local model
([Providers](./providers.md), [Local models](./local-models.md)). Quit the
TUI when that's done.

## 2. Open the app

```bash
koda app
```

The first time, this puts Koda.app in `~/Applications` and opens it. From
then on you can open it from Launchpad, Spotlight or the Dock.

## 3. Pick a folder and ask

The app opens on **New session**: an empty conversation with the prompt box.
Three controls sit under the box:

- **The folder** Koda works in. It shows the last folder you worked in, or
  **Choose a folder** the first time. Click it for the folders you've used
  and **Open folder…**.
- **The model.** Leave it, or click it to pick another.
- **The mode.** **Auto** edits files and runs sandboxed commands without
  asking; **Plan** only reads and plans. Start in Plan if you'd rather see a
  plan first ([Trust modes](./approval.md)).

Type what you want, for example `explain how the tests are organised`, and
press Enter. The session starts with that prompt and appears in the list on
the left, under its folder.

## 4. Watch it work

The reply streams in. What Koda does on the way (reading files, searching,
running commands, editing) folds into one quiet line such as "Read 3 files,
ran 2 commands". Click the line to see each call and what it printed.

While a turn runs, Enter adds a message to it, and **Stop** (or Esc) ends it.

## 5. Answer when Koda needs you

If Koda asks a question, or wants to do something outside the sandbox, a card
marked **Needs your input** appears above the prompt box. Pick an answer and
press **Submit**. If the app isn't in front, a system notification tells you.

## 6. Review what changed

After a turn that edited files, press Shift+Cmd+D. **Changes** opens beside
the conversation with each file and its added and removed lines. To ask for
a fix, hover a line, click the **+** beside its number, type a comment, and
press Cmd+Enter to send your comments to Koda
([Reviewing changes](./desktop-changes.md)).

## 7. Leave and come back

Closing the app stops nothing: a running turn goes on in the background
server. Open the app again and the session is in the list, as you left it.
The same session is in the terminal too, from that folder:

```bash
koda resume --last
```

## 8. Put finished sessions away

Hover a session in the list, open its **…** menu and choose **Archive**. It
leaves the list but isn't deleted; **Archived**, under Search, has it
([Archiving a session](./desktop-sessions.md#archiving-a-session)).

## Next

- [Sessions and the window](./desktop-sessions.md): several sessions side by
  side, Back and Forward, search.
- [Working in a session](./desktop-conversation.md): views, the model menu,
  the side panel and the Activity panel.
- [Settings and shortcuts](./desktop-settings.md).
