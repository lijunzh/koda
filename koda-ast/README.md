# koda-ast

MCP server for tree-sitter AST analysis, part of the [Koda](https://github.com/lijunzh/koda) AI coding agent.

Extracts function signatures, class definitions, and call graphs from source
code using embedded tree-sitter parsers. Communicates via the
[Model Context Protocol](https://modelcontextprotocol.io) over stdio.

## Supported languages

- **Rust**: `.rs`
- **Python**: `.py`, `.pyi`, `.pyw`
- **JavaScript/TypeScript**: `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.tsx`
- **Go**: `.go`
- **Java**: `.java`
- **C/C++**: `.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`
- **Bash**: `.sh`, `.bash`

Additional languages require adding tree-sitter grammars to this crate — see [#298](https://github.com/lijunzh/koda/issues/298).

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

Exposes one MCP tool: **AstAnalysis** — extracts functions, classes, and
call graphs from source files.

## License

MIT
