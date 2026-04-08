---
name: debug
description: Diagnose koda issues — checks provider config, API connectivity, settings, and environment.
tags: [debug, diagnostics, troubleshooting, config]
when_to_use: Use when koda is misbehaving — wrong provider, API errors, tools not working, unexpected behaviour. Describe the issue and this skill will guide a systematic diagnosis.
argument_hint: [issue description, e.g. "API calls are failing with 401"]
allowed_tools: [Read, Grep, Glob, List, Bash]
user_invocable: true
---

# Debug: Diagnose Koda Issues

Help the user diagnose an issue they're encountering with koda. Work through the checklist below, run each command, and report findings. Do not skip steps — a step that passes is still useful information.

## Issue Description

The user's issue: {{args}}

If no issue was described, read the config, logs, and environment and summarise anything that looks wrong.

## Step 1: Session Log

Koda writes a rolling daily log to `~/.config/koda/logs/`. Read the most recent file:

```bash
# Find the latest log file
ls -t ~/.config/koda/logs/ | head -5

# Tail the last 50 lines (logs may be sparse until issue #758 lands)
tail -50 ~/.config/koda/logs/koda.log.$(date +%Y-%m-%d) 2>/dev/null \
  || tail -50 ~/.config/koda/logs/$(ls -t ~/.config/koda/logs/ | head -1) 2>/dev/null \
  || echo "(no log file found)"
```

Search for errors and warnings:
```bash
grep -E "ERROR|WARN" ~/.config/koda/logs/koda.log.$(date +%Y-%m-%d) 2>/dev/null | tail -20
```

Note: logs are currently sparse — instrumentation is being improved in issue #758. If the log is empty, proceed to the steps below.

## Step 2: Environment and API Keys

Check which provider is configured and whether its key is present:

```bash
# Show relevant env vars (mask actual key values)
env | grep -E "KODA|OPENAI|ANTHROPIC|GEMINI|GROQ|MISTRAL|DEEPSEEK|FIREWORKS|TOGETHER|API_KEY|API_BASE" | sed 's/=.*/=<set>/'
```

Common issues:
- Key env var not set → `export OPENAI_API_KEY=...` (or whichever provider)
- Key set but for wrong provider → check `~/.config/koda/settings.toml` for the active provider
- Custom base URL pointing to unreachable endpoint

## Step 3: Settings File

Read the koda settings file:

```bash
cat ~/.config/koda/settings.toml 2>/dev/null || echo "(no settings.toml found — using defaults)"
```

Check for:
- `provider` — is it what the user expects?
- `model` — does it exist on that provider?
- `base_url` — is it reachable?

## Step 4: Agent and Skill Config

List any user-level agent or skill overrides:

```bash
ls ~/.config/koda/agents/ 2>/dev/null && echo "---agents above---" || echo "(no user agents)"
ls ~/.config/koda/skills/ 2>/dev/null && echo "---skills above---" || echo "(no user skills)"
```

If custom agents exist, read any that seem relevant to the issue.

## Step 5: API Connectivity Test

Test raw connectivity to the configured provider endpoint. Use the base URL from settings.toml or the provider default:

```bash
# OpenAI-compatible (OpenAI, Groq, Fireworks, local LM Studio, etc.)
curl -s -o /dev/null -w "%{http_code}" \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  https://api.openai.com/v1/models 2>&1 | head -5

# Anthropic
curl -s -o /dev/null -w "%{http_code}" \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  https://api.anthropic.com/v1/models 2>&1
```

Expected: HTTP 200. Common failures:
- 401 → wrong or missing API key
- 403 → key exists but no access to model
- 000 or connection refused → network issue, wrong base URL, VPN required

## Step 6: Memory Files

Check whether memory files exist and are well-formed:

```bash
wc -l ~/.config/koda/memory.md 2>/dev/null || echo "(no global memory)"
wc -l ./MEMORY.md 2>/dev/null || echo "(no project memory)"
wc -l ./CLAUDE.md 2>/dev/null || echo "(no CLAUDE.md)"
```

Large memory files (>500 lines) can cause context overflow. If suspiciously large, read the first 50 lines.

## Step 7: Summarise Findings

After running all steps, provide:

1. **Root cause** (if found) — be specific
2. **Fix** — exact command or change needed
3. **If unresolved** — what additional information to collect:
   - Run koda with `RUST_LOG=koda_core=debug,koda_cli=debug koda ...` and share the resulting log file
   - Or set `RUST_LOG=debug` for maximum verbosity (very noisy)
