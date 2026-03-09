# Security Model

## Approval Modes

Koda has three approval modes that control tool execution:

| Mode | Description | Default |
|------|-------------|--------|
| **Auto** | Phase-gated approval. Mutations auto-approved during Executing with an approved plan. Destructive operations always require confirmation. | ✅ |
| **Strict** | Every non-read action requires explicit user confirmation. | |
| **Safe** | Local-read-only. No filesystem mutations. Remote actions (GitHub CLI, MCP readOnly) allowed. | |

## Tool Effect Classification

Every tool call is classified into one of four effect levels:

| Effect | Description | Auto | Strict | Safe |
|--------|-------------|------|--------|------|
| **ReadOnly** | No side-effects (file reads, grep, git status) | ✅ auto | ✅ auto | ✅ auto |
| **RemoteAction** | Remote-only side-effects (gh CLI, WebFetch) | ✅ auto | ✅ auto | ✅ auto |
| **LocalMutation** | Local filesystem writes (Write, Edit, cargo test) | ✅ phase-gated | ⚠️ confirm | ❌ blocked |
| **Destructive** | Irreversible (rm -rf, git push --force, sed -i) | ⚠️ confirm | ⚠️ confirm | ❌ blocked |

## Guarantee Matrix

| Action | Auto | Strict | Safe |
|--------|------|--------|------|
| Read files inside project | ✅ | ✅ | ✅ |
| Read files outside project | ✅ (log) | ✅ | ✅ |
| Write files inside project | ✅ (phase-gated, budget-limited) | ⚠️ confirm | ❌ blocked |
| Write files outside project | ⚠️ confirm | ⚠️ confirm | ❌ blocked |
| Delete files | ⚠️ confirm | ⚠️ confirm | ❌ blocked |
| Safe bash (grep, git status) | ✅ | ✅ | ✅ |
| Bash with write side-effect (>, tee) | ✅ (phase-gated) | ⚠️ confirm | ❌ blocked |
| Destructive bash (rm -rf, force push) | ⚠️ confirm | ⚠️ confirm | ❌ blocked |
| Bash with path escape (cd /tmp) | ⚠️ confirm | ⚠️ confirm | ❌ blocked |
| Sub-agent invocation | ✅ | ✅ | ✅ |
| Sub-agent writes | ✅ (DelegationScope) | ⚠️ confirm | ❌ blocked |
| MCP tool (readOnly: true) | ✅ | ✅ | ✅ |
| MCP tool (readOnly: false) | ✅ (phase-gated) | ⚠️ confirm | ❌ blocked |
| MCP tool (config override) | Per override | Per override | Per override |
| MemoryWrite | ✅ (phase-gated) | ⚠️ confirm | ❌ blocked |
| WebFetch (GET) | ✅ | ✅ | ✅ |
| gh issue/pr (RemoteAction) | ✅ | ✅ | ✅ |

## Hardcoded Safety Floors

These checks apply regardless of mode or phase:

1. **Writes outside project root** → NeedsConfirmation (Auto/Strict) or Blocked (Safe)
2. **Bash path escape** (cd /tmp, absolute paths outside project) → NeedsConfirmation
3. **Destructive operations** (Delete, rm -rf, force push) → NeedsConfirmation in Auto
4. **Symlink escape** → canonicalize() resolves symlinks before path checks

## Sub-Agent Delegation

When a parent agent spawns a sub-agent, a `DelegationScope` constrains it:

- **Mode clamping**: Child mode can never exceed parent (Safe parent → Safe child only)
*Filesystem grant**: `ReadOnly`, `Scoped { read_paths, write_paths }`, or `FullProject`
- **Tool allowlist**: Optional list of permitted tools
- **Delegation depth**: `can_delegate` controls whether sub-agents can spawn further sub-agents

Enforcement is a **hard gate** in `check_tool()`, not a log. A compromised sub-agent can only write to paths explicitly granted.

## MCP Tool Classification

MCP tools are classified from their schema annotations:

- `readOnlyHint: true` → ReadOnly (auto-approved in all modes)
- `destructiveHint: true` → Destructive (confirm in Auto+Strict)
- Neither → LocalMutation (conservative default)

Config overrides in `.mcp.json` take precedence:

```json
{
  "toolOverrides": {
    "github.create_issue": "RemoteAction",
    "filesystem.write": "LocalMutation"
  }
}
```

## Simple-Task Action Budget

When the LLM takes the simple-task shortcut (skips planning), it gets an **action budget**:

- Default: 3 LocalMutation/Destructive actions
- ReadOnly and RemoteAction don't count against the budget
- When budget exhausted: system injects a plan-requirement message
- Forces the LLM to produce a plan before continuing

## Accepted Risks

These are gaps intentionally not addressed:

### 1. No kernel-level sandboxing

File write blocking is in-process, not enforced by seccomp/landlock. A malicious LLM output could theoretically call `libc::write` directly via compiled code.

**Mitigation**: This requires the LLM to generate + compile + execute an exploit, which is detectable by the human reviewing tool calls.

### 2. Shell command parsing is heuristic

Complex shell pipelines, subshells, and eval tricks can bypass the command classifier. Example: `bash -c "$(echo cm0gLXJmIC8= | base64 -d)"` would decode and execute `rm -rf /`.

**Mitigation**: Unknown commands are treated as unsafe by default. The `DANGEROUS_PATTERNS` list catches common evasion techniques (`eval`, backticks, `$()`). Complex pipelines require user confirmation in Strict mode.

### 3. MCP readOnly is trust-based

We trust the MCP server's schema declaration. A malicious MCP server could declare `readOnly: true` and still mutate state.

**Mitigation**: Config overrides let users distrust specific tools. MCP servers are explicitly configured by the user.

### 4. Auto mode sub-agents with FullProject scope

If the LLM doesn't narrow the scope when delegating, a prompt-injected sub-agent has full project write access.

**Mitigation**: This is the user's chosen trade-off by selecting Auto mode. The DelegationScope infrastructure exists for narrowing when the LLM is instructed to do so.

### 5. Hardcoded floors don't respect Safe mode's Blocked behavior

The outside-project and path-escape hardcoded floors return `NeedsConfirmation` in all modes, including Safe. In Safe mode, this means the user sees a confirmation prompt for outside-project writes instead of a clean block.

**Mitigation**: In Safe mode, the confirmation dialog doesn't have an "approve" action, so the user can't accidentally approve. The behavior is conservative but the UX could be improved in a future release by returning `Blocked` when mode is Safe.
