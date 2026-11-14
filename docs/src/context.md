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

### Compacting after a turn

Auto-compact runs before a model call, so a reply that fills the context is
compacted when the next turn starts. To compact as soon as the reply is in,
set `KODA_POST_TURN_COMPACT_PERCENT` to the usage, from 1 to 100, past which a
turn compacts after its final reply, before it ends:

```bash
export KODA_POST_TURN_COMPACT_PERCENT=85
```

It is off by default, as Codex's `model_post_turn_compact_threshold_percent`
is. It doesn't run when the turn was stopped or you typed while the reply
streamed, and a failure keeps the reply.

After a pause of 5 minutes or more, Koda also clears old tool results from the
context. This makes no model call.

## Manual compact

Run `/compact` at any time to compact early, e.g. before starting a large
refactor so you have the full context window available. It runs like a turn:
Esc stops it and leaves the history as it was.

## The stored record

`/compact` keeps the replaced messages in the database, marked as compacted,
and Koda never rewrites a stored message, so a session's record is what the
model saw and said. Deleting a session from `/resume` removes it with its
messages.
