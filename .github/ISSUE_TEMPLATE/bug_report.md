---
name: Bug Report
about: Something isn't working right
labels: ["bug"]
---

## What happened?
<!-- One sentence. What did you expect vs what actually happened? -->


## Steps to reproduce
<!-- Minimal steps to trigger the bug. Include commands, prompts, or config. -->

```bash
koda -p "your prompt here"
```

## Error output
<!-- Paste the exact error message or unexpected output. -->

```

```

## Environment
- **Koda version + sandbox state:** <!-- output of `koda --version` (one line, includes sandbox availability) -->
- **OS:** <!-- e.g., macOS 15.2 arm64, Ubuntu 24.04, Windows 11 -->
- **Provider/model:** <!-- e.g., anthropic / claude-sonnet-4-20250514 -->
- **Mode:** <!-- safe / auto (Plan is sub-agent-only, not selectable at top level) -->

## Sandbox-related (only fill in if your bug involves trust/Auto/sandbox)
<!--
  These fields help triage trust-mode and Auto-mode-related issues (#860).
  Skip if your bug is unrelated to sandboxing.
-->
- **Did koda print a sandbox warning at startup?** <!-- yes / no / didn't notice -->
- **Did `koda --mode auto` refuse to start?** <!-- yes (paste the error) / no / didn't try -->

## Relevant files
<!-- Which source files are involved? Helps LLM-assisted triage. -->
<!-- e.g., src/approval.rs, src/tools/shell.rs -->


## Additional context
<!-- Screenshots, log snippets, or config excerpts. Optional. -->
