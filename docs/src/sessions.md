# Session management

Koda stores every conversation in a local SQLite database, organised by
project root. Each session gets a UUID that you can use to resume it.

```bash
# List and pick a session interactively
/resume

# Resume by ID prefix from the TUI
/resume a3f8bc

# Resume from the command line, with the full ID: in the TUI, or headless
koda -s a3f8bc12-1587-4c6d-a855-46183ca84fba
koda exec -s a3f8bc12-1587-4c6d-a855-46183ca84fba "continue where we left off"

# Delete a session permanently
/resume delete a3f8bc
```

## Away summary

When you resume a session that was idle, Koda shows:

- How long you were away
- Message and tool-call counts
- Total tokens used
- A banner if the previous turn was interrupted mid-inference

## Session title

In the TUI, the first 50 characters of your first message become the session's
title, shown in `/resume`.
