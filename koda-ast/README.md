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

## Built-in integration

koda-ast is compiled into Koda as a direct library call — no setup needed.
Just ask koda to analyze code structure and it works out of the box.

The standalone MCP server binary is also available for use in other editors.

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
