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

Add to `.mcp.json`:
```json
{
  "mcpServers": {
    "email": {
      "command": "koda-email",
      "args": [],
      "env": {
        "IMAP_HOST": "imap.gmail.com",
        "IMAP_USER": "you@gmail.com",
        "IMAP_PASS": "app-password",
        "SMTP_HOST": "smtp.gmail.com",
        "SMTP_USER": "you@gmail.com",
        "SMTP_PASS": "app-password"
      }
    }
  }
}
```

## MCP tools exposed

| Tool | Description |
|------|-------------|
| `EmailRead` | Read emails from inbox or specified folder |
| `EmailSearch` | Search emails by subject, sender, date, or body |
| `EmailSend` | Compose and send emails |

## License

MIT
