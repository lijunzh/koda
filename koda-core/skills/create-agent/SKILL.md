---
name: create-agent
description: Scaffold a new sub-agent JSON file (.koda/agents/<name>.json) with the correct schema, tool scoping, and trust mode.
tags: [agent, subagent, scaffold, create, meta]
when_to_use: Use when the user asks to create a new sub-agent, custom agent, specialist, or worker — anything that should be invokable via InvokeAgent.
allowed_tools: [Read, Write, Glob, List, AskUser]
---

# Create Agent

You are scaffolding a new koda sub-agent. The goal: produce a working
`.koda/agents/<name>.json` (or `~/.config/koda/agents/<name>.json`) that
the user can immediately invoke via `InvokeAgent`.

## Principles

- **Read before writing.** Open `koda-core/agents/explore.json` (or
  `plan.json`, `verify.json`, `task.json`) to ground yourself in the
  canonical patterns before generating the file.
- **Declare `trust` explicitly.** This is the #1 footgun. Pre-#1250
  agents used `write_access` (now deprecated). Today every new agent
  must declare its natural `trust` mode — the kernel and approval
  matrix derive everything from it. Skipping `trust` falls back to
  legacy default-deny which silently strips Write/Edit/Delete and
  causes opaque "no such tool" failures.
- **Principle of least privilege.** Pick the lowest trust mode that
  works: `plan` for pure search/analysis, `safe` for write-capable
  workers, never `auto` (auto is reserved for the user-controlled
  top-level session).
- **One agent, one purpose.** A good `description` is a single sentence
  that lets the main agent know when to invoke it.

## Trust modes (the only knob you really need)

| Mode    | What's allowed                                           | When to pick it                                                           |
| ------- | -------------------------------------------------------- | ------------------------------------------------------------------------- |
| `plan`  | Read-only. Kernel-enforced sandbox blocks all mutations. | Explorers, planners, reviewers, analyzers. Anything that only reads.      |
| `safe`  | Reads + sub-agent-context auto-approved Write/Edit.       | Workers that modify files. Default for write-capable sub-agents.          |
| `auto`  | Reads + writes + (with caveats) network actions.         | **Don't.** Reserved for the top-level interactive `koda` agent only.      |

Destructive ops (`rm -rf`, `git reset --hard`, `git push --force`) are
**always** blocked in sub-agents regardless of trust — the safe-side
rule. If the user wants those, they invoke them top-level with their
own consent.

## Process

### 1. Clarify intent (if ambiguous)

Use `AskUser` only when the request is genuinely ambiguous. Skip it for
obvious requests like "make me a test-running agent." Things worth asking
about when unclear:

- **Read-only or write-capable?** ("Will this agent modify files?")
  Read-only → `trust: "plan"`. Write-capable → `trust: "safe"`.
- **Project or personal scope?** Project = `.koda/agents/<name>.json`,
  personal/cross-project = `~/.config/koda/agents/<name>.json`
- **Cheap model or default?** Default uses the main agent's model;
  `gemini-2.5-flash` saves cost for high-volume work.

### 2. Choose the file path

| Scope    | Path                                          | When                                                  |
| -------- | --------------------------------------------- | ----------------------------------------------------- |
| Project  | `.koda/agents/<name>.json`                    | Workflow specific to this repo, shared with the team. |
| Personal | `~/.config/koda/agents/<name>.json`           | Cross-project specialist that follows the user.       |

**Note:** Personal path is `~/.config/koda/`, **not** `~/.koda/`.

### 3. Pick the right shape

#### Read-only specialist (modeled after `explore`)

```json
{
  "name": "reviewer",
  "description": "Read-only code reviewer. Use when asked to critique code without making changes.",
  "system_prompt": "You are a senior code reviewer. Find bugs, anti-patterns, and improvements. Do NOT modify files.",
  "trust": "plan",
  "skip_memory": true
}
```

Use this shape for: explorers, reviewers, planners, analyzers, anything
that only reads. `trust: "plan"` is kernel-enforced read-only — strictly
stronger than `disallowed_tools` denylisting (the model can't bypass
the sandbox even with creative tool calls). `skip_memory: true` saves
tokens since read-only agents don't need memory context.

#### Write-capable worker (modeled after `task`)

```json
{
  "name": "refactor",
  "description": "Refactors code per user instructions while preserving behavior.",
  "system_prompt": "You refactor code while preserving behavior. Run tests after each change.",
  "trust": "safe",
  "model": "gemini-2.5-flash"
}
```

Use this shape for: refactorers, fixers, generators, anything that
modifies the workspace. `trust: "safe"` auto-approves Write/Edit when
invoked as a sub-agent (sub-agents have no live human channel for
approval prompts), while still blocking destructive ops. `model`
override lets you save cost for high-volume workers.

#### Read-only-with-execution (modeled after `verify`)

```json
{
  "name": "tester",
  "description": "Runs the test suite and reports failures. Does NOT modify files.",
  "system_prompt": "You run tests and report results. You can run any read-only command including builds and tests, but never write files.",
  "trust": "safe",
  "disallowed_tools": ["Write", "Edit", "Delete"]
}
```

Rare but useful escape valve: `trust: "safe"` allows mutating Bash
(needed for `cargo test`, `npm test`, `make`), but the explicit
`disallowed_tools` denylist forbids file mutations. Pre-#1250 this was
implicit; post-#1250 you must spell it out because Safe trust no
longer auto-blocks Write/Edit on its own.

### 4. Field reference

**Required:**

- `name` — agent identifier (used with `InvokeAgent`)
- `system_prompt` — behavioral instructions for the LLM
- `trust` — `"plan"`, `"safe"`, or `"auto"`. **Always declare this.**

**Highly recommended:**

- `description` — one-line purpose; shown in the main agent's sub-agent
  listing. Without this, the main agent has no signal for when to invoke.

**Tool scoping (pick at most one approach, in addition to `trust`):**

- `allowed_tools` — allowlist; empty (default) = all tools available
- `disallowed_tools` — denylist; behavioral floor for tools the trust
  matrix can't gate (`InvokeAgent`, `AskUser`, `TodoWrite` are
  classified as ReadOnly and need explicit denylisting if you want a
  read-only agent that doesn't spawn sub-agents or ask questions)

**Memory:**

- `skip_memory` — default `false`. Set `true` for read-only agents to
  avoid injecting MEMORY.md into the system prompt (saves tokens).

**Model overrides (optional — omit to inherit main agent settings):**

- `model` — e.g. `"gemini-2.5-flash"` for cheap workers
- `provider`, `base_url` — for non-default providers
- `max_tokens`, `temperature`, `thinking_budget`, `reasoning_effort`
- `max_context_tokens`, `max_iterations`

**Deprecated (do not use in new agents):**

- `write_access` — superseded by `trust`. Old agents with
  `write_access: true` still work but emit a deprecation warning at
  load. Migrate to `trust: "safe"`.

### 5. Write and confirm

1. Use `Write` to create the file at the chosen path.
2. Tell the user:
   - Where the file was saved
   - How to invoke it: `InvokeAgent("<name>", "<task>")`
   - That they can edit the JSON directly to refine it

## Anti-patterns to avoid

- **Don't omit `trust`.** Without it, the loader falls back to legacy
  default-deny which silently strips Write/Edit/Delete and causes
  opaque "no such tool" failures. This is the #1 footgun. Always
  declare `trust` explicitly.
- **Don't set `trust: "auto"` on a sub-agent.** Auto is reserved for
  the top-level user-controlled session. The kernel won't let a
  sub-agent escalate above its parent's trust anyway (`derive_child_trust`
  clamps `min(parent, declared)`), so declaring auto is misleading at
  best and silently downgraded at worst.
- **Don't put both `allowed_tools` and `disallowed_tools`.** Pick one.
  When `allowed_tools` is non-empty, `disallowed_tools` is redundant.
- **Don't set the agent's `model` to something the user hasn't
  configured.** If they're on Anthropic, `gemini-2.5-flash` won't work
  unless they've set up Gemini credentials too.
- **Don't write a 500-word `system_prompt`.** Bundled agents like
  `explore.json` keep it focused — 1-3 paragraphs of role + constraints
  + workflow.

## Reference

For a battle-tested example, read `koda-core/agents/explore.json`. It
demonstrates: `trust: "plan"` for kernel-enforced read-only,
`skip_memory: true` for token savings, and a tight `description` that
the main agent can reason about. For a write-capable example, read
`task.json` (`trust: "safe"`).
