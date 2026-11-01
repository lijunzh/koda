# Privacy & data

Koda has **zero telemetry**. No usage data, crash reports, or analytics
are collected or transmitted anywhere.

## What stays local

- Conversations are stored **only** on your machine: in your local SQLite
  database, and in the log when you turn it up to debug level
- API keys are stored locally in the same database (file mode 0600)
- Koda sends data only to the provider you chose, and to the sites and MCP
  servers its tools reach when the model calls `WebFetch`, `WebSearch` or an
  MCP tool
- Version checks query crates.io only (no Koda-specific server)
- With an Anthropic key, the Claude aliases ask Anthropic's Models API for the
  newest model ids
- You can audit every byte sent to the model: `RUST_LOG=koda_core::providers=trace`
  logs each request body (see [Debugging a run](./debugging.md))

## Database location

```text
~/.config/koda/db/koda.db
```

The database is a standard SQLite file. You can inspect it with any
SQLite browser or the `sqlite3` CLI.

## Deleting your data

```bash
# Delete all sessions
rm ~/.config/koda/db/koda.db

# Or selectively from the TUI
/sessions delete <id>
```

There is no cloud backup. Deleted data is gone.
