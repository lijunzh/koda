# Upgrading to 0.8

Koda 0.8 removes Safe mode and decides what asks the way Codex does: from the
kernel sandbox, not from a list of commands. This page lists what you may need
to change coming from 0.7. The
[changelog](https://github.com/lijunzh/koda/blob/main/CHANGELOG.md) has
everything, release by release.

## Safe mode is gone

There are two modes, **Auto** (the default) and **Plan**; see
[Trust modes](./approval.md).

| In 0.7 | In 0.8 |
|---|---|
| `--mode safe`, `-c mode=safe` | Refused: `--mode` takes `auto` or `plan` |
| `mode = "safe"` in `config.toml` | Koda stops with "unknown variant `safe`, expected `plan` or `auto`"; remove the line or pick one |
| `"trust": "safe"` in an agent file | Runs in Plan, with a warning; write `"auto"` for an agent that edits |
| A session last run in Safe | Reopens in Auto |
| `Shift+Tab` cycles Safe, Auto, Plan | Switches between Auto and Plan |
| `/safe` | Gone |
| `a` at an approval (approve and switch to Auto) | Gone |
| "Implement this plan?" offers Safe or Auto | "Yes, implement it" switches to Auto |

## What asks in Auto

Everything inside the sandbox runs without asking: edits, and any command,
including `rm`, `sed -i` or `chmod` on project files. These ask first:

- **A destructive command:** a forced `rm` anywhere, `git reset --hard`,
  `git clean -f`, force pushes (`--force`, `--force-with-lease`, `+branch`),
  deleting a remote branch, `npm publish` and `cargo publish`, `gh` merges and
  deletes, `sudo`, disk tools.
- **A command that asks to leave the sandbox** (`outside_sandbox`).
- **An edit of Koda's own settings** (`~/.config/koda`).
- **A host that isn't on the proxy's allow-list** (below).

Two things that asked in 0.7 don't any more:

- **A command naming a path outside the project** (`cd ~ && ls`,
  `cat ../notes.md`). The sandbox already keeps a command's writes in the
  project and temp directories, as in Codex.
- **Any change, in Safe**, since Safe is gone.

## Git commits leave the sandbox

`.git` is read-only inside the sandbox, as in Codex: `git status`, `log` and
`diff` run, but `git add`, `commit`, `checkout` and `stash` fail there. Koda
asks to run them outside the sandbox. Answer `s` to allow the prefix it shows
for the rest of the session: `` [s] `git commit …` this session `` covers every
later `git commit`, though never a destructive command.

## Hosts that aren't on the list

Commands still reach the network only through Koda's proxy, which allows
GitHub and the main package registries. Another host used to fail; now Koda
asks, on the command that reached for it:

```
  Bash  Network access to docs.rs:443 (not on the allow-list)
  [y] approve  [s] docs.rs this session  [n] reject  [f] feedback  [esc] stop
```

`y` lets that one connection through, `s` allows the host for the session, and
`n` stops the command. A headless run refuses the host, as before. See
[Sandbox](./sandbox.md#a-host-that-isnt-on-the-list).

## Plan runs commands

Plan used to run only commands on a read-only list. It now runs every command
in a read-only sandbox: a command can look at anything and change nothing.
Edits are still refused.

## A machine with no sandbox

Where the kernel sandbox isn't available (no `bwrap` on Linux, say), Auto
used to refuse to start. Now Koda starts, and Auto asks before every command
and edit; Plan refuses commands. `koda exec` refuses every command and edit
there, since nobody can approve them. Installing the sandbox brings back
Auto's usual behaviour.

## Removed flags

The flags deprecated in 0.7 are gone:

| In 0.7 | In 0.8 |
|---|---|
| `--max-tokens N` | `-c model_max_output_tokens=N` |
| `--temperature X` | `-c model_temperature=X` |
| `--thinking-budget N` | `-c model_thinking_budget=N` |

Each can also go in `config.toml`.

## For clients of the protocol

- `TrustMode` has no `safe`.
- `item/toolCall/requestApproval` has two new fields: `sessionPrefix`, what
  `acceptForSession` covers for a command leaving the sandbox, and
  `networkApprovalContext` (`host`, `protocol`), as Codex's, for a host off the
  allow-list. See [Headless mode](./headless.md#the-client-protocol).
