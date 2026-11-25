# Quickstart

A first session, start to finish: ask about a project, plan a change, make
it, review it, and come back to it later. It assumes Koda is
[installed](./installation.md). Any project works; a git repository makes
reviewing the change easier.

## 1. Open Koda in your project

```bash
cd ~/src/my-project
koda
```

The first time, Koda opens its provider picker. Choose a provider and paste
its API key, or give a local server's address. Or skip the key altogether:
`koda --oss` runs on a local model it starts for you (see [Local
models](./local-models.md)). Koda keeps the key in its database on your
machine; you won't be asked again.

The status bar at the bottom shows the trust mode (`⚡ AUTO`), whether the
sandbox is on (`🛡 sandboxed`), the model, and how much of its context is
left.

## 2. Give Koda the project's instructions

```text
/init
```

The model reads the project and writes an `AGENTS.md`: what the project is,
how to build and test it, and its conventions. Koda reads that file into
every request from then on, so the model doesn't rediscover it each time.
Read it, cut what isn't true or isn't needed, and commit it; short is
better ([Memory](./memory.md) explains why).

## 3. Ask a question

```text
> how does a request get from the HTTP handler to the database?
```

You see the model work: each tool call (a `grep` through `Bash`, a `Read`) appears as it
runs, then the answer. Reads never ask for approval. Mention a file with `@`
to hand it over directly: `@src/db.rs why is this query slow?`

## 4. Plan a change

For anything bigger than a small fix, plan first. `Shift+Tab` cycles the
trust mode to **Plan**, or type:

```text
/plan add a --dry-run flag to the import command
```

In Plan the model can only look. It explores, asks you what it can't find
out, and proposes a plan, drawn as a framed **Proposed plan**. Koda then asks
**Implement this plan?**:

- **in Safe:** each change asks you first.
- **in Auto:** changes inside the project run without asking, in the sandbox.
- **No, keep planning:** answer with what to change, and it revises.

## 5. Let it make the change

Pick one. In **Safe**, each edit and command shows what it will do and waits:

| Key | Does |
|---|---|
| `y` | Run this one |
| `s` | Run it, and don't ask again this session for the same file or command |
| `n` | Don't run it; the model tries something else |
| `f` | Don't run it, and tell the model why |
| `Esc` | Stop the turn |

In **Auto**, edits and builds inside the project just run; Koda still asks
before anything destructive (`rm -rf`, `git push --force`) or outside the
project. The model usually builds and tests what it changed; if it doesn't,
ask it to.

## 6. Review it

```text
/diff
```

Shows the uncommitted changes, then offers a review by the model or a commit
with a message it writes. Or read the diff in your own tools; Koda changes
files in place, like any editor.

## 7. Come back later

Quit with `Ctrl+C` twice or `/exit`. The session is saved. To continue it:

```bash
koda resume --last        # the latest session in this project
koda resume               # pick one from the list
```

`/new` starts a fresh conversation without leaving Koda; the old one stays
in `/resume`.

## 8. Run it without the TUI

For scripts and CI, `koda exec` runs one prompt and exits:

```bash
koda exec "update the changelog for the --dry-run flag"
git diff | koda exec "review this"
```

See [Headless mode](./headless.md).

## Next

- [How Koda works](./how-koda-works.md): the loop, tools, approval and context
- [Trust modes](./approval.md) and the [sandbox](./sandbox.md): what Koda may
  do without asking
- [Slash commands](./commands.md) and [keybindings](./keybindings.md)
