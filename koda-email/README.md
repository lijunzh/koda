# koda-email

MCP server for email integration, part of the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Read, search, and send email via IMAP/SMTP. Works with any email provider
(Gmail, Outlook, FastMail, self-hosted). Communicates via the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio.

## Auto-provisioning

Koda auto-installs and connects this server when email access is needed.
Just ask — "check my email" — and koda handles the rest.

On first use, you'll be prompted for IMAP/SMTP credentials.

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
> (`/provider` wizard stores credentials encrypted at `~/.config/koda/keys`).

## MCP tools exposed

| Tool | Description |
|------|-------------|
| `EmailRead` | Read emails from inbox or specified folder |
| `EmailSearch` | Search emails by subject, sender, date, or body |
| `EmailSend` | Compose and send emails |

## License

MIT
