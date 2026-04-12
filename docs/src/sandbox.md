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

The following directories and files are blocked from both read and write
access, preventing the model from exfiltrating secrets:

**Directories:**
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
- `~/.config/koda/db` — Koda's SQLite DB (contains API keys)

**Files:**
`~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.docker/config.json`, `~/.vault-token`, `~/.env`

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
