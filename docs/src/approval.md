# Trust modes

Koda has a single permission knob — **TrustMode** — that controls how
tool calls are gated. Toggle with `Shift+Tab` (current mode shown in
the status bar).

## Modes

| Mode | Status bar | Behavior |
|------|-----------|----------|
| **Safe** | 🔒 Safe | Confirm every side effect. Read-only tools auto-approved. User default. |
| **Auto** | ⚡ Auto | Auto-approve all actions within the project sandbox. Only writes outside the project root still require confirmation. |

A third mode, **Plan** (📋 Plan), is available for agent definitions
that need investigation-only access — all writes are denied, not just
confirmed.

## Sandbox is always active

Unlike older versions, the kernel sandbox (macOS seatbelt / Linux bwrap)
with credential protection is **always enforced**. There is no opt-out.
The sandbox is the safety boundary — the trust mode only controls whether
you see a confirmation prompt before each mutation.

## What the sandbox blocks

**Writes** restricted to: project directory, `/tmp`, and standard cache
dirs (`~/.cache`, `~/.cargo`, etc.).

**Credential directories** blocked from read+write:
`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.azure`,
`~/.password-store`, `~/.terraform.d`, `~/.config/gcloud`,
`~/.config/gh`, `~/.config/op`, `~/.config/helm`,
`~/.config/koda/db`

**Credential files** blocked:
`~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.docker/config.json`, `~/.vault-token`, `~/.env`

**Agent-file protection**: `.koda/agents/` and `.koda/skills/` are
write-protected in all modes to prevent prompt injection.

## Trust mode × tool effect matrix

| Tool effect | Plan | Safe | Auto |
|-------------|------|------|------|
| ReadOnly | ✅ auto | ✅ auto | ✅ auto |
| LocalMutation | ❌ deny | ⚠️ confirm | ✅ auto |
| RemoteAction | ❌ deny | ⚠️ confirm | ✅ auto |
| Destructive | ❌ deny | ⚠️ confirm | ✅ auto |
| Outside project | ❌ deny | ⚠️ confirm | ⚠️ confirm |

## Approval keys

When a confirmation prompt appears:

| Key | Effect |
|-----|--------|
| `y` | Approve this one action |
| `n` | Reject this one action |
| `a` | Approve and enable Auto mode for the rest of the session |
| `f` | Reject and provide written feedback the model can act on |
| `Esc` | Reject (same as `n`) |

## Headless mode

In headless mode there is no human to prompt. The trust mode is
effectively Auto: read-only and write tools are approved, destructive
Bash commands are rejected, and the sandbox enforces the perimeter.
