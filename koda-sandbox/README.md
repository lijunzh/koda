# koda-sandbox

> Capability-aware sandbox layer for [Koda](https://github.com/lijunzh/koda).
> Kernel-enforced FS / network / exec policies, derived from trust mode at compile time.

This crate owns *kernel-level* sandbox enforcement (Seatbelt on macOS, bwrap on Linux),
workspace provisioning, and the egress proxy. It implements the design in
[#934](https://github.com/lijunzh/koda/issues/934).

---

## Table of contents

1. [Threat model](#threat-model)
2. [Trust modes](#trust-modes)
3. [Architecture at a glance](#architecture-at-a-glance)
4. [Per-platform enforcement](#per-platform-enforcement)
5. [Defense in depth](#defense-in-depth)
6. [Escape hatches](#escape-hatches)
7. [Configuration philosophy](#configuration-philosophy)
8. [What's enforced today](#whats-enforced-today)
9. [Security boundary tests](#security-boundary-tests)

---

## Threat model

### What we defend against (in scope)

| Threat | Layer | Notes |
|---|---|---|
| **LLM-issued path traversal** (`../../etc/passwd`, symlink escape) | App-layer `path_defense` + kernel | Symlink chains expanded before policy check |
| **LLM-issued credential exfiltration via `cat ~/.ssh/id_rsa`** | Kernel deny-read on `koda/db` only; egress proxy filters traffic | Reading other creds is *intentionally* allowed (CLI tools need them) — see [accepted risks](#what-we-explicitly-do-not-defend-against-out-of-scope) |
| **LLM-issued unauthorized writes** (modify `~/.ssh/authorized_keys`, drop a `.bashrc` payload) | Kernel deny-write on credential dirs + project carve-outs | `koda/db` is fully read+write denied |
| **LLM-issued egress to attacker domains** | App-layer egress proxy (HTTP/HTTPS/SOCKS5) | TLS termination optional via corporate CA |
| **LLM-issued sandbox escape via `git config core.fsmonitor`** | `fs.allow_git_config = false` by default | Prevents registering a malicious hook → RCE |
| **Runaway / DoS commands** (`yes \| head -c 1G`, fork bombs) | Wall-time ceiling + output cap in shell tool | Per-trust resource limits in policy |
| **Sub-agent privilege escalation** | `SandboxPolicy::compose` — child can only narrow parent | Anti-escalation tests pinned per field |
| **Worker process compromise** | Crash-isolated `koda-fs-worker` per slot | Compromised worker can't take down the koda host |

### What we explicitly do **not** defend against (out of scope)

These are **accepted risks** with documented rationale — not bugs. Changes
need an issue + design discussion before tightening, because each one
exists to keep legitimate workflows working.

| Out-of-scope | Why we don't enforce | Mitigated by |
|---|---|---|
| **Reading credential files** (`~/.ssh/id_rsa`, `~/.aws/credentials`) | CLI tools (`gh`, `aws`, `kubectl`, `ssh`) authenticate by reading their own config. Blocking reads breaks normal use. | Egress proxy (Phase 3) prevents exfiltration *over the network*. Blocking reads without blocking egress is security theater (see [#855](https://github.com/lijunzh/koda/issues/855)). |
| **Local filesystem readout of arbitrary `~/Documents`** | The agent operates on the user's project; tight read scoping would break grep across symlinked dotfiles, monorepos, pnpm stores. | Trust mode + user gate (Plan / Safe). Reads stay local until the egress proxy decides. |
| **CPU spikes** | Wall-time + output caps catch most foot-guns; CPU rlimits add platform-specific gnarliness. | Wall-time ceiling (`MAX_TIMEOUT_SECS = 300s`) bounds blast radius. PR-6 will add CPU rlimits. |
| **Side-channel attacks** (timing, cache) | Out of scope for an LLM agent sandbox. Threat model is *anti-malice*, not *anti-research*. | n/a |
| **Privileged-user attacks** | The user runs koda — they could just `rm -rf` themselves. The sandbox prevents the *LLM* from doing it, not the user. | Trust model: user is trusted, LLM is not. |
| **Kernel bugs in Seatbelt / bwrap** | Trusting the kernel's own sandbox primitives is unavoidable. | Defense in depth via app-layer checks; report platform vulns upstream. |

### Trust assumptions

- **The user is trusted.** They invoked koda intentionally. Sandbox protects against LLM-issued actions, not user-issued ones.
- **The kernel is trusted.** We rely on Seatbelt / bwrap correctness for FS isolation.
- **The LLM is *not* trusted.** Treat every tool call as potentially adversarial — the agent might be jailbroken, prompt-injected, or just hallucinating dangerous commands.
- **The koda host process is trusted.** It runs *outside* the sandbox and orchestrates worker processes that run *inside*.
- **Worker processes are crash-isolation boundaries.** A compromised worker dies; the koda host respawns and continues.

---

## Trust modes

The user's chosen trust mode is the **only** runtime input that affects sandbox
behavior. Every dial in `SandboxPolicy` is derived from it at compile time via
`koda_core::sandbox::policy_for_agent`. See [Configuration philosophy](#configuration-philosophy).

| Mode | Approval | Wall time | Deny-search depth | Use case |
|---|---|---|---|---|
| **Plan** | All mutations rejected | 60s | 3 (perf-tuned) | Read-only exploration, code review |
| **Safe** | Each mutation user-approved | 60s | 5 (balanced) | Default for general dev work |
| **Auto** | Auto-approved | 60s | 10 (max paranoia) | CI / unattended runs — *kernel sandbox required* |

**Security argument for the depth scaling:** more permissive trust mode → less
human gating → more paranoid sandbox checking. Auto mode runs without a user
in the loop, so the sandbox compensates with the deepest deny-rule traversal.

This invariant is mechanically pinned by the test
`policy_for_agent_depth_is_strictly_monotone_with_permissiveness` in
`koda-core/src/sandbox.rs`.

---

## Architecture at a glance

```text
                 ┌──────────────────────────────────────────────────┐
                 │  koda host process (TRUSTED, runs outside sbox)  │
                 │                                                  │
                 │  • LLM provider calls                            │
                 │  • Tool dispatch                                 │
                 │  • SandboxPolicy construction (policy_for_agent) │
                 │  • SandboxPool slot allocation                   │
                 └────┬───────────────────────┬─────────────────────┘
                      │ spawns                │ spawns
                      │ (Seatbelt/bwrap)      │ (Unix socket IPC)
                      ▼                       ▼
        ┌──────────────────────┐   ┌─────────────────────────────────┐
        │ User command         │   │ koda-fs-worker (crash-isolated) │
        │ (LLM-controlled)     │   │                                 │
        │                      │   │  • Validates Write/Edit ops     │
        │  Kernel-enforced:    │   │    against SandboxPolicy        │
        │  • FS deny rules     │   │  • Receives policy via env var  │
        │  • Network egress    │   │    KODA_FS_WORKER_POLICY (IPC)  │
        │  • Process limits    │   │  • One worker per sandbox slot  │
        └──────────────────────┘   └─────────────────────────────────┘
                      │
                      ▼ outbound traffic
        ┌──────────────────────────────────────┐
        │ Built-in egress proxy (Phase 3)      │
        │  • Domain allow/deny lists           │
        │  • Optional MITM via corporate CA    │
        │  • SOCKS5 fallback for non-HTTP apps │
        └──────────────────────────────────────┘
```

### Dependency direction

```text
koda-cli → koda-core → koda-sandbox
                          │
                          └─ no upward dependency
```

`koda-sandbox` knows nothing about `Persistence`, `Provider`, or
`ToolRegistry`. Pure infrastructure — testable with
`assert_eq!(transform(cmd, policy), expected)`.

---

## Per-platform enforcement

| Platform | Mechanism | Module | Enforcement scope |
|---|---|---|---|
| **macOS** | `sandbox-exec(1)` (Seatbelt) | `seatbelt.rs` | FS read/write, network egress, process limits |
| **Linux** | `bwrap(1)` (bubblewrap) | `bwrap.rs` + `stage2.rs` | FS via mount namespaces, network via netns + bridge |
| **Other Unix** | None | n/a | Refuses to enter Auto mode (`failIfUnavailable`) |
| **Windows** | None today | n/a | Out of scope for current threat model |

### macOS Seatbelt details

- Profile generated dynamically per-policy — see `seatbelt::build_profile_string`.
- `policy_overlay_rules` appends user policy *after* the hardcoded baseline so
  the baseline (credential paths, koda DB) acts as a floor.
- Optional MITM proxy chaining via `build_proxied_profile_string` for corporate
  Zscaler / PKI environments.
- `weaker_macos_isolation` flag (default `false`) opts into Apple `trustd`
  callbacks for Go-binary TLS verification — costs network sandboxing strength.

### Linux bwrap details

- Stage-2 helper (`koda-sandbox-stage2`) sets up the in-netns TCP↔UDS bridge
  *inside* the unshare'd namespace, then `execvp`s the user's command.
- Mount namespace gives true read-only `/` with carved-out writable roots.
- Network namespace + virtual bridge gives kernel-enforced egress: even
  binaries that ignore `HTTPS_PROXY` env vars cannot escape via direct TCP.

### Auto mode is fail-closed

When the user picks Auto, koda **refuses to start** if the kernel sandbox
isn't available on the platform (Codex's `failIfUnavailable` pattern).
Rationale: Auto means "no human in the loop" — running without kernel
enforcement under those conditions would be reckless.

---

## Defense in depth

The sandbox is **not** a single fence — it's four concentric ones, each
correcting for failure modes the others can't see.

| Layer | Catches | Module |
|---|---|---|
| **1. Trust mode + approval gate** | Operator-policy violations *before* they reach the sandbox | `koda-core::trust` |
| **2. Application-layer path defense** | Symlink escapes, `..` chains, dangerous-system-path heuristics | `path_defense.rs` |
| **3. SandboxPolicy compose chain** | Sub-agent privilege escalation (child can only narrow parent) | `policy.rs::compose` |
| **4. Kernel sandbox** | Direct syscalls that bypass app-layer checks | `seatbelt.rs` / `bwrap.rs` |

A bypass needs to defeat *all four* layers, not any one. This is by design —
each layer's threat model is documented in its module-level rustdoc.

---

## Escape hatches

These are **intentional bypass mechanisms**. They exist for specific
documented use cases. Each one has a security cost; using one is a deliberate
acknowledgement that the user accepts that cost.

### `--no-sandbox` flag

Disables kernel sandbox entirely. Used for:
- **Debugging the sandbox itself** when developing `koda-sandbox`.
- **Platform-unsupported environments** (CI containers without `bwrap`).
- **Reproducing customer-reported bugs** where the user can't run with sandbox.

> **Cost:** No FS / network / exec enforcement. Only the trust-mode approval
> gate stands between the LLM and your filesystem. Plan mode still rejects
> mutations, but Safe and Auto allow whatever the LLM asks for.

### `fs.allow_git_config = true`

Permits writes to `git config`. Off by default because `git config core.fsmonitor`
can register a hook that runs arbitrary code on the next `git status` → RCE.

> **Cost:** Trades sandbox containment for `git config` ergonomics. Only
> opt in for workflows that genuinely need to modify git config (rare).

### `net.weaker_macos_isolation = true`

macOS-only. Permits Apple `trustd` mach lookups so Go binaries can verify TLS.

> **Cost:** Widens the network sandbox surface. Opt in if Go binaries don't
> work otherwise.

### MITM CA bundle (`mitm.ca_bundle`)

Configures the egress proxy to terminate TLS using a corporate CA (Zscaler,
internal PKI). Used for environments where the corporate proxy is mandatory
and inspects HTTPS.

> **Cost:** All HTTPS traffic from sandboxed processes is decrypted and
> re-encrypted by koda. The proxy still applies domain allow/deny rules —
> MITM is *enforcement*, not an escape — but it materially expands what
> koda's process can see.

### `failIfUnavailable = false` (implicit, for Plan/Safe modes)

Plan and Safe modes start *without* a kernel sandbox if the platform doesn't
support one. Auto mode requires kernel enforcement.

> **Cost:** Plan/Safe rely entirely on app-layer path defense + the approval
> gate when no kernel sandbox is available. Trust mode is the safety net.

---

## Configuration philosophy

> **Koda is config-free at runtime.**

Every behavioral dial in `SandboxPolicy` is derived at compile time from the
trust mode (Plan / Safe / Auto) via `koda_core::sandbox::policy_for_agent`.
There is no JSON config file, no CLI override, no env-var dial that the user
can twiddle.

**Why.** Config knobs are foot-guns:

- They turn into compatibility liabilities the moment they ship.
- They force users to make security decisions they're not equipped to make.
- They drift from documented defaults, breaking the threat model silently.

If a behavioral change is justified, it goes into `policy_for_agent` (a
compile-time table keyed off trust mode), not into a config file.

### One legitimate exception: IPC

`SandboxPolicy` keeps its `serde::Deserialize` impl because the
crash-isolated `koda-fs-worker` process needs to receive the policy across
the process boundary (via `KODA_FS_WORKER_POLICY` env var as JSON). This is
**IPC, not config** — the host built the policy in-memory via
`policy_for_agent`; the JSON is just the wire format.

To prevent this IPC channel from morphing into a config loader by accretion,
the test
`sandbox_policy_deserialize_only_used_in_fs_worker_binary` walks the
workspace and **fails the build** if any file other than the worker binary
deserializes a `SandboxPolicy`. A future contributor adding a config loader
gets a red CI build with a teaching moment baked in.

---

## What's enforced today

Honest accounting of which fields actually do something vs. are
declared-but-not-yet-enforced. This list is meant to drift downward over time
as enforcement code lands.

| Field | Declared | Enforced |
|---|---|---|
| `fs.deny_read` | ✅ | ✅ kernel (Seatbelt + bwrap) + worker |
| `fs.allow_read_within_deny` | ✅ | ✅ kernel + worker |
| `fs.allow_write` | ✅ | ✅ kernel + worker |
| `fs.deny_write_within_allow` | ✅ | ✅ kernel + worker |
| `fs.allow_git_config` | ✅ | ✅ worker |
| `fs.mandatory_deny_search_depth` | ✅ | ✅ worker — clamped via `effective_chain_depth` floor=8 / ceiling=40 |
| `net.allowed_domains` / `net.denied_domains` | ✅ | ✅ egress proxy |
| `net.allow_local_binding` | ✅ | ✅ kernel |
| `net.mitm` | ✅ | ✅ egress proxy |
| `net.weaker_macos_isolation` | ✅ | ✅ Seatbelt profile |
| `limits.wall_time_secs` | ✅ | ✅ shell tool dispatch |
| `limits.cpu_time_secs` | ✅ | ✅ setrlimit RLIMIT_CPU — SIGXCPU at cap |
| `limits.max_rss_bytes` | ✅ | ✅ setrlimit RLIMIT_AS — ENOMEM at cap (Linux RLIMIT_RSS is a no-op; AS caps virtual memory) |
| `limits.max_open_fds` | ✅ | ✅ setrlimit RLIMIT_NOFILE — EMFILE at cap |
| `limits.max_output_bytes` | ✅ | ✅ shell tool dispatch (line cap) |
| `trust` (TrustPreference) | ✅ | ✅ approval gate (`koda-core::trust`) |

Declared-only fields are intentionally inert; setting them today has no
runtime effect. They're included in `SandboxPolicy` so the type contract
stays stable while enforcement lands incrementally.

---

## Security boundary tests

Each load-bearing claim in this README is pinned by a test. If you change
the corresponding behavior, expect to update the test by name — that's the
point.

| Claim | Test |
|---|---|
| Auto mode refuses to run without kernel sandbox | `koda-core::sandbox::tests::build_errors_in_auto_mode_when_sandbox_unavailable` |
| Sub-agent can never widen parent's policy | `koda-sandbox::policy::tests::compose_drops_child_allow_writes_so_child_cannot_widen` |
| Trust mode is strictly monotonic with paranoia | `koda-core::sandbox::tests::policy_for_agent_depth_is_strictly_monotone_with_permissiveness` |
| Wall-time ceiling clamps even policy-supplied values | `koda-core::tools::shell::tests::timeout_max_ceiling_clamps_policy_too` |
| Symlink escape is denied at the worker | `koda-sandbox::worker::tests::symlink_escape_is_denied` |
| `koda/db` is fully read-write denied | `koda-sandbox::seatbelt::tests::strict_profile_fully_denies_koda_db` |
| Only the worker binary deserializes `SandboxPolicy` | `koda-sandbox::policy::tests::sandbox_policy_deserialize_only_used_in_fs_worker_binary` |
| Egress proxy clamps Auto mode network | `koda-sandbox::seatbelt::tests::proxied_profile_omits_open_network_allow` |

Adding a new defended threat? Add a test next to the relevant module and
update this table. Removing a defended threat? Update the [threat
model](#threat-model) table first, then remove the test.

---

## Phase status

This crate is the kernel-enforcement implementation of issue
[#934](https://github.com/lijunzh/koda/issues/934). Phase 5 (current) wraps
sub-agent policy composition + the per-trust derivation. PR-6 (next) wires
declared-only resource limits to actual rlimit / SBPL enforcement.

For the full design rationale, threat-model evolution, and per-phase
acceptance criteria, see the issue.
