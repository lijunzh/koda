# Privacy & data

Koda has **zero telemetry**. No usage data, crash reports, or analytics
are collected or transmitted anywhere.

## What stays local

- Conversations are stored **only** in your local SQLite database
- API keys are stored locally in the same database (file mode 0600)
- The only network traffic is your LLM API calls to the provider you chose
- Version checks query crates.io only (no Koda-specific server)
- You can audit every byte sent to the model by reading the DB directly

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
