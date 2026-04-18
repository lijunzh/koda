---
name: create-agent
description: Scaffold a new sub-agent JSON file (.koda/agents/<name>.json) with the correct schema, tool scoping, and write-access settings.
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
  `plan.json`, `verify.json`) to ground yourself in the canonical patterns
  before generating the file.
- **Read-only by default.** `write_access` defaults to `false`. If the
  user describes any task that creates, edits, or deletes files, set
  `write_access: true` explicitly. **This is the #1 footgun.** An agent
  meant to "fix bugs" without `write_access: true` will silently lack
  Write/Edit/Delete and fail opaquely.
- **Principle of least privilege.** Use `disallowed_tools` to forbid
  tools the agent shouldn't have, and prefer cheap models (`gemini-2.5-flash`)
  for high-volume worker agents.
- **One agent, one purpose.** A good `description` is a single sentence
  that lets the main agent know when to invoke it.

## Process

### 1. Clarify intent (if ambiguous)

Use `AskUser` only when the request is genuinely ambiguous. Skip it for
obvious requests like "make me a test-running agent." Things worth asking
about when unclear:

- **Read-only or write-capable?** ("Will this agent modify files?")
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
  "disallowed_tools": ["Write", "Edit", "Delete"],
  "skip_memory": true
}
```

Use this shape for: explorers, reviewers, planners, analyzers, anything
that only reads. `skip_memory: true` saves tokens since read-only agents
don't need memory context.

#### Write-capable worker

```json
{
  "name": "refactor",
  "description": "Refactors code per user instructions while preserving behavior.",
  "system_prompt": "You refactor code while preserving behavior. Run tests after each change.",
  "write_access": true,
  "model": "gemini-2.5-flash"
}
```

Use this shape for: refactorers, fixers, generators, anything that
modifies the workspace. `model` override lets you save cost for
high-volume workers.

### 4. Field reference

**Required:**

- `name` — agent identifier (used with `InvokeAgent`)
- `system_prompt` — behavioral instructions for the LLM

**Highly recommended:**

- `description` — one-line purpose; shown in the main agent's sub-agent
  listing. Without this, the main agent has no signal for when to invoke.

**Tool scoping (pick at most one approach):**

- `allowed_tools` — allowlist; empty (default) = all tools available
- `disallowed_tools` — denylist; useful for locking down read-only agents

**Write/memory:**

- `write_access` — default `false`. Set `true` only when the agent must
  modify files. Without it, Write/Edit/Delete are silently unavailable.
- `skip_memory` — default `false`. Set `true` for read-only agents to
  avoid injecting MEMORY.md into the system prompt (saves tokens).

**Model overrides (optional — omit to inherit main agent settings):**

- `model` — e.g. `"gemini-2.5-flash"` for cheap workers
- `provider`, `base_url` — for non-default providers
- `max_tokens`, `temperature`, `thinking_budget`, `reasoning_effort`
- `max_context_tokens`, `max_iterations`

### 5. Write and confirm

1. Use `Write` to create the file at the chosen path.
2. Tell the user:
   - Where the file was saved
   - How to invoke it: `InvokeAgent("<name>", "<task>")`
   - That they can edit the JSON directly to refine it

## Anti-patterns to avoid

- **Don't omit `write_access: true` for write-capable agents.** Silent
  tool unavailability is the worst kind of bug.
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
demonstrates: read-only locking via `disallowed_tools`, `skip_memory`
for token savings, and a tight `description` that the main agent can
reason about.
