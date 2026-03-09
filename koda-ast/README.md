# koda-ast

MCP server for tree-sitter AST analysis, part of the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Extracts function signatures, class definitions, and call graphs from source
code using embedded tree-sitter parsers. Communicates via the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio.

## Built-in languages

Rust, Python, JavaScript, TypeScript.

## Auto-provisioning

Koda auto-installs and connects this server when AST analysis is needed.
No manual setup required — just ask koda to analyze code structure.

## Manual setup

```bash
cargo install koda-ast
```

Add to `.mcp.json`:
```json
{
  "mcpServers": {
    "ast": {
      "command": "koda-ast",
      "args": []
    }
  }
}
```

## MCP tools exposed

| Tool | Description |
|------|-------------|
| `AstAnalysis` | Extract functions, classes, and call graphs from source files |

## License

MIT
