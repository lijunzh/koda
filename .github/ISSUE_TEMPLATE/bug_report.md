---
name: Bug Report
about: Something isn't working right
labels: ["bug"]
---

## What happened?
<!--
  One sentence. What did you expect vs what actually happened?
  Debugging guide: https://lijunzh.github.io/koda/debugging.html
-->


## Steps to reproduce
<!-- Minimal steps to trigger the bug. Include commands, prompts, or config. -->

```bash
koda exec "your prompt here"
```

## Error output
<!-- Paste the exact error message or unexpected output. -->

```

```

## Environment
- **Koda version + sandbox state:** <!-- output of `koda --version` (one line, includes sandbox availability) -->
- **OS:** <!-- e.g., macOS 15.2 arm64, Ubuntu 24.04, Windows 11 -->
- **Provider/model:** <!-- e.g., anthropic / claude-haiku-5-5 -->
- **Mode:** <!-- safe / auto (Plan is sub-agent-only, not selectable at top level) -->

## Logs
<!--
  What's in these: https://lijunzh.github.io/koda/debugging.html#what-to-put-in-a-bug-report
  Debug logs and bundles hold your prompts and the files Koda read. Check them
  before you post them. Never attach koda.db: it holds your API keys.
-->
- **Log:** <!-- paste from `koda-logs --session <id>` (`koda-logs --backfill 300` for the newest lines), or attach <log_dir>/koda.log if you set log_dir -->
- **Feedback report:** <!-- TUI: attach the .zip /feedback saves (say yes to logs if you can) -->
- **`exec/summary`:** <!-- headless: the last line of koda exec --json -->
- **Lines that show the problem:** <!-- by name, e.g. compaction branch="cut_off", turn end outcome="stopped" -->

## Sandbox-related (only fill in if your bug involves trust/Auto/sandbox)
<!--
  These fields help triage trust-mode and Auto-mode-related issues (#860).
  Skip if your bug is unrelated to sandboxing.
-->
- **Did koda print a sandbox warning at startup?** <!-- yes / no / didn't notice -->
- **Did `koda --mode auto` refuse to start?** <!-- yes (paste the error) / no / didn't try -->

## Relevant files
<!-- Which source files are involved? Helps LLM-assisted triage. -->
<!-- e.g., koda-core/src/compact.rs, koda-core/src/tools/shell.rs -->


## Additional context
<!-- Screenshots, log snippets, or config excerpts. Optional. -->
