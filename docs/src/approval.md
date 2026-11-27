# Trust modes

Koda decides what runs without asking the way Codex does: from the kernel
sandbox. A command that stays inside the sandbox runs; one that needs more
asks to leave it, and you decide. The **trust mode** picks the sandbox:
`Shift+Tab` in the TUI switches between the two, and the status bar shows
the current one as a colour-coded badge.

## The two modes

| Mode | Badge | What runs without asking |
|------|-------|--------------------------|
| **Auto** | ⚡ **AUTO** (bold green) | Everything inside the workspace-write sandbox: the project and scratch directories are writable, `.git` and credential files are read-only, and the network goes through Koda's proxy. A destructive command, a command that asks to leave the sandbox, and a path outside the project ask first. **The default.** |
| **Plan** | 📋 PLAN (cyan) | Reads, searches and shell commands, in the read-only sandbox: a command can look at anything and change nothing. Edits are refused, and so is a destructive command or one asking to leave the sandbox. |

`--mode` and `mode` in `config.toml` choose the mode a session starts in.
Headless runs that should only look use `koda exec --mode plan "..."` (see
[Headless mode](./headless.md#read-only-runs)).

Until 0.8.0 there was a third mode, Safe, which asked before every change.
It is gone, as Codex has none: the sandbox, not a list of commands, decides
what a command can do. A config with `mode = "safe"` gets a message saying so,
and a session stored in Safe reopens in Auto.

## Planning before changing anything

Plan mode is for working out what to do with the model before it does
anything, as in Codex and Claude Code. Enter it with `/plan`, or with
`/plan <prompt>` to send a prompt at once, or with `Shift+Tab`.

In Plan, the model is told to explore the project, ask you about what it
can't find out, and then propose a plan in a `<proposed_plan>` block,
which the TUI draws as a framed **Proposed plan**. Koda then asks
**Implement this plan?**:

- **Yes, implement it**: switches to Auto and sends "Implement the plan."
- **No, keep planning**: stays in Plan. Typing a reply does the same.

Plan stays read-only throughout: the host refuses any edit, and every `Bash`
command runs with the project read-only in the kernel sandbox. A session left
in Plan reopens in Plan.

## What asks in Auto

| Call | Auto | Plan |
|------|------|------|
| Read, search, a command inside the sandbox | ✅ runs | ✅ runs (read-only sandbox) |
| Edit, Write | ✅ runs | ❌ refused |
| A destructive command (`rm -rf`, `git push --force`, …) | ⏸ asks | ❌ refused |
| A command asking to leave the sandbox (`outside_sandbox`) | ⏸ asks | ❌ refused |
| Edit or Write in a temp directory (`/tmp`, `$TMPDIR`) | ✅ runs | ❌ refused |
| Edit or Write of Koda's own settings (`~/.config/koda`) | ⏸ asks | ❌ refused |
| Edit or Write anywhere else outside the project | ❌ refused | ❌ refused |

**Leaving the sandbox.** A Bash call with `outside_sandbox: true` runs without
the kernel sandbox; the model makes one when the sandbox blocks something you
need, and the prompt shows its reason. The commonest is git: the sandbox keeps
`.git` read-only, as Codex's does, so `git add`, `git commit` and
`git checkout` fail inside it, and the model asks to run them outside. With
the request it can propose a `prefix_rule`, such as `["git", "commit"]`, which
your "this session" answer covers (below). See
[Sandbox](./sandbox.md#when-a-command-needs-to-leave-the-sandbox).

**Which commands are destructive** follows Codex: a forced `rm` (`rm -f`,
`rm -rf`, `rm --force`) is the one local file operation that asks, even
behind `env`, `sudo`, `xargs` or `trap`, inside `sh -c`, `eval` or
`find -exec`, or spelled `/bin/rm`. Koda adds what the sandbox can't contain
or what loses work for good: `git reset --hard`, `git clean -f`, force pushes
(`--force`, `--force-with-lease`, `+refspec`), deleting a remote branch
(`--delete`, `:branch`, `--mirror`), `npm publish` and `cargo publish`, `gh`
merges, deletes, `auth` and `api`, `sudo`, disk tools and reboots. Git's own
options (`git -C dir`, `git -c key=value`) don't hide a command. Anything
else, a plain `rm`, `sed -i` or `chmod` included, changes only what the
sandbox lets it, so it runs without asking.

**A machine with no sandbox.** Without a working kernel sandbox (no `bwrap` on
Linux, say), nothing contains a command, so in Auto every command and every
edit asks first, as Claude Code does with its sandbox off, and Plan refuses
shell commands. The TUI status bar shows the sandbox state (🛡 sandboxed /
⚠ unsandboxed) next to the badge, and `/feedback`'s diagnostics record it.

## Sub-agents ask in the main session

A sub-agent's tool calls go through the same rules as the main agent's, in
the session's mode, as Claude Code's sub-agents inherit the session's mode and
rules. What would ask for the main agent asks for a sub-agent too, blocking or
[in the background](./tools.md#background-agents), in the main session:

```
  🤖 task a1b2c3d4e5f6 asks
  Bash  git commit -m "Fix the parser"
  [y] approve  [s] this session  [n] reject  [esc] reject
```

- The request names the agent, as Codex heads one with its thread; a
  background agent's also gives its id.
- **Esc refuses that one call**, and the agent goes on (Claude Code). Esc on
  the main agent's own request still stops the turn.
- **"Allow for session"** covers the whole session: a yes given to a sub-agent
  covers the main agent's same call, and the reverse.
- A background agent can ask after the turn that started it has ended; the
  request waits at the prompt. Several requests at once are shown one after
  another.
- With nobody to ask (`koda exec`), Koda answers as it does for the main
  agent: what the sandbox holds goes ahead, destructive actions and leaving
  the sandbox are refused.

Clients see a sub-agent's request as `item/toolCall/requestApproval` with an
`agent` field (`name`, and a background agent's `id`); outside a turn its
`turnId` is empty.

## Always-on safety floors

These apply in both modes:

1. **Kernel sandbox** (macOS seatbelt / Linux bwrap) restricts file
   writes to the project directory and scratch zones (`/tmp`, `~/.cache`,
   `~/.cargo`, etc.), keeps `.git` read-only, and protects credential
   directories and files. See [Sandbox](./sandbox.md).
2. **Outside-project floor:** Edit and Write work only in the project and
   temp directories, and ask before changing Koda's own settings, which
   decide what the next session runs. A command's paths aren't checked,
   as in Codex: `cd ~ && ls` and `cat ../notes.md` run, a write outside the
   project fails at the kernel, and a forced `rm` asks wherever it points.
3. **Agent-file protection:** `.koda/agents/` and `.koda/skills/` are
   write-protected in every mode, so a prompt injection can't rewrite an
   agent's tools or system prompt mid-session.
4. **Credential scrub:** sandboxed shell calls run with a fixed environment
   allowlist; secrets like `OPENAI_API_KEY`, `AWS_SECRET_ACCESS_KEY` and
   `GITHUB_TOKEN` never reach the child process.

## Approval keys

When a confirmation prompt appears:

| Key   | Effect                                                            |
|-------|-------------------------------------------------------------------|
| `y`   | Approve this one action                                           |
| `s`   | Approve, and don't ask again this session for the same kind of call |
| `n`   | Reject this one action                                            |
| `f`   | Reject and provide written feedback the model can act on          |
| `Esc` / `Ctrl+C` | Stop the turn; the action does not run                |

`n` and `f` let the model carry on and try something else. `Esc` and `Ctrl+C`
stop it until your next prompt, and the model sees that you stopped the action.

### Allow for this session

What `s` covers is what the prompt showed:

- **A command leaving the sandbox:** its `prefix_rule`, as Codex's session
  approval. A yes to `git commit -m "Fix"` with the prefix `git commit`
  covers every later `git commit` that leaves the sandbox. A prefix that would
  cover any program (`bash`, `python -c`, `sudo`, `git` alone and the like,
  Codex's banned list) isn't offered, and a later command that would be
  destructive still asks. The prompt shows what `s` covers:
  `` [s] `git commit …` this session ``.
- **A host off the proxy's allow-list:** the host and port, for every later
  command ([Sandbox](./sandbox.md#a-host-that-isnt-on-the-list)).
- **A web page:** the site.
- **On a machine with no sandbox:** the exact command, or the file for Edit and
  Write.

Destructive actions and anything outside the project ask every time. The
allow lives in memory for this session only, as in Codex. Koda never writes
it to a config file, so a new session or a restart asks again.

## Per-agent trust declaration

Custom agents declare their trust mode in JSON via the `trust` field:

```json
{ "name": "my-reviewer", "trust": "plan", "...": "..." }
```

Valid values: `"plan"` | `"auto"`; an agent that names none runs in Auto.
See [Custom agents](./agents.md) for the full per-agent shape.

## Headless mode

In headless mode there is no human to prompt. Koda applies the headless
policy documented in [Headless mode](./headless.md): what the sandbox holds
runs, destructive commands and leaving the sandbox are refused, and the
sandbox enforces the perimeter.

## Reference

- The approval rules: `koda_core::trust::check_tool`
- Session answers: `koda_core::trust::SessionAllows`
- Sub-agent approvals: `koda_core::approval_flow::Approver`, answered by `koda-server`'s `HostApprover`
- The dangerous-command list: `koda_command::bash_safety`
- Status-bar badge rendering: `koda_tui::widgets::status_bar`
