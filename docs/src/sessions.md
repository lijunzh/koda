# Session management

Koda stores every conversation in a local SQLite database, organised by
project root. Each session gets a UUID that you can use to resume it.

```bash
# List and pick a session interactively
/sessions

# Resume by ID prefix from the TUI
/sessions resume a3f8bc

# Resume from the command line (headless or interactive)
koda -s a3f8bc
koda -s a3f8bc "continue where we left off"

# Delete a session permanently
/sessions delete a3f8bc
```

## Away summary

When you resume a session that was idle, Koda shows:

- How long you were away
- Message and tool-call counts
- Total tokens used
- A banner if the previous turn was interrupted mid-inference

## Session title

Koda auto-generates a short title after the first exchange. The title is
shown in `/sessions` and the status bar.
