# Context management

Every provider has a context window limit (measured in tokens). The status
bar shows current usage as a percentage (e.g. `34%`).

## Auto-compact

When usage reaches **85%**, Koda automatically compacts the session:

1. The newest 30% of the history by size stays as it is (at least 4 messages)
2. The rest is summarised by the model, and your own messages from that part
   are kept word for word beside the summary
3. The old messages are archived in the DB and the summary takes their place
4. A status line appears: `📦 Context at 85% — compacting before sending...`

Usage is Koda's estimate, raised when the provider's last count came in higher,
so the percentage can jump after a reply.

Auto-compact is skipped if there are pending tool calls (it waits for the
turn to finish cleanly).

After a pause of 5 minutes or more, Koda also clears old tool results from the
context. This makes no model call.

## Manual compact

Run `/compact` at any time to compact early, e.g. before starting a large
refactor so you have the full context window available.

## Purging archived history

`/compact` keeps the replaced messages in the DB, archived. Use `/purge` to
delete them:

```text
/purge        ← prompt and delete all archived messages
/purge 90d    ← delete only archived messages older than 90 days
```
