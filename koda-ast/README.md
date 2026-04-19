# koda-ast

Tree-sitter intelligence for the [Koda](https://github.com/lijunzh/koda) AI
coding agent. A pure library — no binary, no MCP server, no async runtime.
Other crates in the workspace embed it directly.

## What's inside

- **`analysis`** — file structure summaries, call graph extraction,
  post-edit syntax verification.
- **`highlight`** — language-agnostic semantic-token syntax highlighting,
  consumed by `koda-cli` for the TUI. Tree-sitter primary backend
  (Phase 2), syntect fallback for unsupported languages.
- **`grammar`** — single-source-of-truth `extension → tree_sitter::Language`
  registry shared by both subsystems.
- **`tokens`** — the `SemanticToken` enum that's the lingua franca for
  renderers.

## Supported languages (tree-sitter)

- **Rust**: `.rs`
- **Python**: `.py`, `.pyi`, `.pyw`
- **JavaScript/TypeScript**: `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.tsx`
- **Go**: `.go`
- **Java**: `.java`
- **C/C++**: `.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`
- **Bash**: `.sh`, `.bash`

For languages without a tree-sitter grammar (toml, yaml, json, markdown,
css, html, sql, …), the highlight pipeline falls back to **syntect**'s
~50 bundled syntaxes. Anything else degrades cleanly to plain text.

Additional tree-sitter grammars: see [#298](https://github.com/lijunzh/koda/issues/298).

## Usage

```rust
use koda_ast::{analyze_file, highlight_spans, syntax_check, LanguageHint};
use std::path::Path;

// Analysis
let summary = analyze_file(Path::new("src/main.rs"))?;
let errors  = syntax_check(Path::new("src/main.rs"));   // None if valid

// Highlighting (returns semantic spans; renderer maps to colors)
let spans = highlight_spans("fn main() {}", LanguageHint::from_extension("rs"));
```

## Testing

```bash
cargo test -p koda-ast
```

## License

MIT
