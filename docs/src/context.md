# Context management

Every provider has a context window limit (measured in tokens). The status
bar shows current usage as a percentage (e.g. `34%`).

## Auto-compact

When usage reaches **85%**, Koda automatically compacts the session:

1. All but the last 4 messages are summarised by the model
2. The summary is stored in the DB (recoverable with `/purge`)
3. A status line appears: `🐻 Context at 85% — auto-compacting…`

Auto-compact is skipped if there are pending tool calls (it waits for the
turn to finish cleanly).

## Manual compact

Run `/compact` at any time to compact early, e.g. before starting a large
refactor so you have the full context window available.

## Purging archived history

`/compact` keeps summaries in the DB. Use `/purge` to delete them:

```text
/purge        ← prompt and delete all archived messages
/purge 90d    ← delete only archived messages older than 90 days
```
