# Sandbox

Koda can sandbox Bash tool commands to prevent the model from accidentally
(or adversarially) reading credentials or writing outside the project.

## Modes

| Mode | Writes | Reads | Network |
|------|--------|-------|---------|
| `none` (default) | Unrestricted | Unrestricted | Unrestricted |
| `project` | Project dir + `/tmp` + cache dirs only | Unrestricted | Unrestricted |
| `strict` | Same as `project` | Blocks credential dirs | Unrestricted |

## Usage

```bash
# CLI flag
koda --sandbox project
koda --sandbox strict

# Environment variable
export KODA_SANDBOX=strict
koda
```

## What's protected in strict mode

**Directories** blocked from read+write:
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

**Files** blocked:
`~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.docker/config.json`, `~/.vault-token`, `~/.env`

## Agent-file protection

In all sandbox modes (`project` and `strict`), writes to `.koda/agents/`
and `.koda/skills/` within the project are blocked. This prevents a
sandboxed command from modifying agent definitions that could alter system
prompts or tool access on the next session.

## Sub-agent inheritance

Child agents inherit the parent's sandbox mode and can never run with
less protection. If the parent runs with `--sandbox strict` but the
agent JSON specifies `sandbox: "project"` (or omits it), the child
still runs with `strict`.

## Platform backends

| Platform | Backend | Install |
|----------|---------|---------|
| macOS | `sandbox-exec` (seatbelt) | Built-in |
| Linux | `bwrap` (bubblewrap) | `apt install bubblewrap` |
| Windows | Not supported | — |

If you request sandboxing but the backend is unavailable, the command
**fails with an error** rather than silently running unsandboxed.
