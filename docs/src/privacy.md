# Privacy & data

Koda has **zero telemetry**. No usage data, crash reports, or analytics
are collected or transmitted anywhere.

## What stays local

- Conversations are stored **only** on your machine: in your local SQLite
  database, and in the log when you turn it up to debug level
- API keys are stored locally in the same database (file mode 0600)
- Your settings (model choice, provider endpoints, MCP servers) are in
  `~/.config/koda/config.toml`, also readable by you only. It holds no API
  keys
- Koda sends data only to the provider you chose, and to the sites and MCP
  servers its tools reach when the model calls `WebFetch`, `WebSearch` or an
  MCP tool. A page `WebFetch` fetches goes to the provider too, for a
  separate model call to read (Haiku on a Claude model)
- The model can search and read your other sessions (`SearchSessions`,
  `ReadSession`), so a session on a remote provider can send there what an
  earlier session said, one that ran on a local model included. A session
  you mark [confidential](./sessions.md#confidential-sessions) is given
  only to [local models](./sessions.md#what-counts-as-a-local-model) (LM
  Studio, Ollama, vLLM or llama.cpp on your own machine or network); its
  forks and a session that reads it are marked too, its sub-agents run
  only on such a model or its own, and its model saves no notes. The mark
  isn't encryption: the session stays in the database as plain text
- Version checks query crates.io only (no Koda-specific server)
- With an Anthropic key, the Claude aliases ask Anthropic's Models API for the
  newest model ids
- You can audit every byte sent to the model: `RUST_LOG=koda_providers=trace`
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

# Delete what you typed (the TUI's ↑ / ↓ history)
rm ~/.config/koda/history.jsonl

# Or selectively from the TUI
/resume delete <id>
```

There is no cloud backup. Deleted data is gone.
