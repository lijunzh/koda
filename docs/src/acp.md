# ACP server (editor integration)

Koda implements the [Agent Client Protocol](https://agentclientprotocol.org)
over stdio JSON-RPC 2.0. This lets editors connect to Koda as a local agent
without network setup.

```bash
# Start the server (editors launch this automatically)
koda server --stdio
```

## Protocol lifecycle

```text
Editor → initialize           (negotiate protocol version)
Koda   ← InitializeResponse

Editor → session/new          (create a session)
Koda   ← NewSessionResponse   (returns session_id)

Editor → session/prompt       (send a user message)
Koda   ← [stream of session/update events]
Koda   ← PromptResponse       (turn complete)

Editor → Cancel               (optional — aborts the running turn)
```

Each line on stdin/stdout is a complete, self-contained JSON-RPC object.

## Editor setup

For VS Code, Zed, and other editors, see your editor's extension docs for
how to configure a local ACP agent. The command to register is:

```text
koda server --stdio
```

No ports, no tokens, no network configuration required.
