# Trust modes

Koda has **one** permission knob — **`TrustMode`** — that controls
whether tool calls execute, get a confirmation prompt, or get blocked
outright. Toggle with `Shift+Tab` in the TUI; the current mode is
shown as a **color-coded badge** in the status bar.

## Mental model in one paragraph

The trust mode is the **single mechanism** for tool gating. Every
permission decision in Koda — whether the master agent can write to
disk, whether a sub-agent can call `Bash`, whether `rm -rf` is
auto-approved — derives from `(trust_mode, tool_effect)`. The kernel
sandbox (macOS seatbelt / Linux bwrap) is the **always-on safety
floor** underneath; the trust mode only decides whether you see a
confirmation prompt before each mutation. There are no separate
"strict mode," "yolo mode," or per-tool toggles to keep in your head.

## The three modes

| Mode | Badge | Mental model |
|------|-------|---------------|
| **Plan** | 📋 PLAN (cyan) | "Investigation only — no side effects." Read tools auto-approve; mutating and destructive tools are **blocked** (not just confirmed). Use for code review, exploration, and dry runs. |
| **Safe** | 🔒 SAFE (yellow) | "Confirm every side effect." Read tools auto-approve; everything that mutates state asks first. The conservative default. |
| **Auto** | ⚡ **AUTO** (inverted black-on-green) | "Trust the sandbox." Read and mutating ops auto-approve within the sandbox; destructive ops (`rm -rf`, `git reset --hard`, `git push --force`, `Delete`) still ask. Outside-project writes still ask. |

The `Auto` badge is **inverted black-on-green** specifically because
Auto is the most-permissive mode and the user must never be unsure
they're in it. Plan and Safe stay as bold colored text — visible but
not as visually loud. (#1232 §8a)

## Trust mode × tool effect matrix (top-level / master agent)

The master agent — i.e. you talking to Koda directly — uses this matrix:

| Tool effect              | Plan      | Safe        | Auto        |
|--------------------------|-----------|-------------|-------------|
| `ReadOnly`               | ✅ auto   | ✅ auto     | ✅ auto     |
| `LocalMutation` (Write/Edit/MemoryWrite) | ❌ deny | ⏸ confirm | ✅ auto |
| `RemoteAction`           | ❌ deny   | ⏸ confirm   | ✅ auto     |
| `Destructive` (Delete, `rm -rf`, force-push, …) | ❌ deny | ⏸ confirm | ⏸ confirm |
| Outside-project write    | ❌ deny   | ⏸ confirm   | ⏸ confirm   |

**Why `Auto × Destructive` confirms** (changed in #1251): the user
said YOLO for normal work, not for `rm -rf`. Destructive ops by
definition can't be undone by the sandbox alone (deleting a tracked
file is "legal" inside the project root), so Auto keeps the prompt
as a deliberate speed-bump.

## Sub-agent matrix (context-sensitive resolution)

Sub-agents (anything dispatched via `InvokeAgent`) have **no live
human approval channel** — by design. The master agent's TUI is the
only confirm-prompt surface; sub-agents run headlessly and can't
"ask" anyone. So the sub-agent matrix resolves what the master would
treat as `⏸ confirm` using a **safe-side rule**:

| Tool effect              | Sub-agent in Plan | Sub-agent in Safe | Sub-agent in Auto |
|--------------------------|-------------------|-------------------|-------------------|
| `ReadOnly`               | ✅ auto           | ✅ auto           | ✅ auto           |
| `LocalMutation`          | ❌ deny           | ✅ auto           | ✅ auto           |
| `RemoteAction`           | ❌ deny           | ✅ auto           | ✅ auto           |
| `Destructive`            | ❌ deny           | ❌ block          | ❌ block          |
| Outside-project write    | ❌ deny           | ❌ block          | ❌ block          |

**The asymmetry on the "ask" cells**: in Safe mode, mutating ops
**auto-approve** (the user already trusted this sub-agent enough to
spawn it; nagging would be useless without a UI to nag in), but
destructive ops **block** (we still want a backstop on the worst
ops, even when no one's home to confirm). This is the bug fix from
#1249 — pre-#1251, every Write from a Safe-trust sub-agent was
auto-rejected with *"requires user confirmation but this sub-agent
has no channel to the user."*

The sub-agent matrix is implemented in
`koda_core::trust::check_tool_for_sub_agent`; the master matrix is
`check_tool`. Both are pure functions with the same signature
otherwise.

## Always-on safety floors

These apply **regardless of the trust mode**:

1. **Kernel sandbox** (macOS seatbelt / Linux bwrap) restricts file
   writes to the project directory + scratch zones (`/tmp`,
   `~/.cache`, `~/.cargo`, etc.) and protects credential dirs/files.
   See [Sandbox](./sandbox.md).
2. **Outside-project floor** — writes to paths outside the project
   root always confirm (Safe + Auto) or deny (Plan), even if the
   matrix would otherwise auto-approve.
3. **Sandbox-unavailable downgrade** — if the platform backend isn't
   installed (e.g. `bwrap` missing on Linux), Auto downgrades
   `LocalMutation` and `Destructive` to `⏸ confirm` so you never lose
   both the sandbox **and** the prompt at the same time. (#860)
4. **Agent-file protection** — `.koda/agents/` and `.koda/skills/`
   are write-protected in every mode to prevent prompt injection
   from rewriting an agent's tools or system prompt mid-session.
5. **Credential scrub** — sandboxed shell calls run with a fixed env
   allowlist; secrets like `OPENAI_API_KEY`, `AWS_SECRET_ACCESS_KEY`,
   `GITHUB_TOKEN` never reach the child process. (#1228)

## Approval keys

When a confirmation prompt appears:

| Key   | Effect                                                            |
|-------|-------------------------------------------------------------------|
| `y`   | Approve this one action                                           |
| `n`   | Reject this one action                                            |
| `a`   | Approve **and** enable Auto mode for the rest of the session      |
| `f`   | Reject and provide written feedback the model can act on          |
| `Esc` | Reject (same as `n`)                                              |

## Per-agent trust declaration

Custom agents declare their trust mode in JSON via the `trust` field:

```json
{ "name": "my-reviewer", "trust": "plan", "...": "..." }
```

Valid values: `"plan"` | `"safe"` | `"auto"`. See [Custom agents](./agents.md)
for the full per-agent shape.

The legacy `write_access: bool` field is **deprecated** — pre-existing
JSONs continue to work (a warning is logged at load), but new agents
should use `trust:` directly. The new field is strictly more
expressive: it captures *kernel sandbox bounds* + *per-tool approval
rules* + *sub-agent context-sensitive defaults* in one declaration,
where `write_access` only spoke to the second half.

## Headless mode

In headless mode there is no human to prompt. The trust mode is
effectively "Auto with the destructive backstop": read and mutating
tools approve, destructive Bash commands and `Delete` are rejected,
and the sandbox enforces the perimeter. See [Headless mode](./headless.md).

## Reference

- Master matrix: `koda_core::trust::check_tool`
- Sub-agent matrix: `koda_core::trust::check_tool_for_sub_agent`
- Sandbox-unavailable fallback: `koda_core::trust::is_outside_project`
  + #860 downgrade in `trust.rs`
- Per-agent loader & deprecation warning: `koda_core::config::KodaConfig::load`
- Status-bar badge rendering: `koda_cli::widgets::status_bar`
