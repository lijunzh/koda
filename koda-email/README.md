# koda-email

MCP server for email integration, part of the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Read, search, and send email via IMAP/SMTP. Works with any email provider
(Gmail, Outlook, FastMail, self-hosted). Communicates via the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio.

## Built-in integration

koda-email is compiled into Koda as a direct library call — no setup needed.
Just ask “check my email” and koda handles the rest.

On first use, you’ll be prompted for IMAP/SMTP credentials.

The standalone MCP server binary is also available for use in other editors.

## Manual setup

```bash
cargo install koda-email
```

Add to `.mcp.json` (use env var references — don't hardcode credentials):
```json
{
  "mcpServers": {
    "email": {
      "command": "koda-email",
      "args": [],
      "env": {
        "IMAP_HOST": "imap.gmail.com",
        "IMAP_USER": "$EMAIL_USER",
        "IMAP_PASS": "$EMAIL_PASS",
        "SMTP_HOST": "smtp.gmail.com",
        "SMTP_USER": "$EMAIL_USER",
        "SMTP_PASS": "$EMAIL_PASS"
      }
    }
  }
}
```

> **⚠️ Security:** Never hardcode email credentials in `.mcp.json` — if that
> file is committed to a repo, your inbox is exposed. Set `EMAIL_USER` and
> `EMAIL_PASS` as environment variables or use koda's built-in keystore
> (`/key` wizard stores credentials in the SQLite KV store at `~/.config/koda/db/koda.db`, file mode 0600).

## MCP tools exposed

| Tool | Description |
|------|-------------|
| `EmailRead` | Read emails from inbox or specified folder |
| `EmailSearch` | Search emails by subject, sender, date, or body |
| `EmailSend` | Compose and send emails |

## License

MIT
