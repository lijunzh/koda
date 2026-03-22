# Contributing to Koda

Koda is a personal AI assistant built in Rust. Contributions are welcome.

## Quick Start

```bash
git clone https://github.com/lijunzh/koda.git
cd koda
cargo build
cargo test --workspace --features koda-core/test-support
```

## Project Structure

Four-crate workspace:

| Crate | Role |
|-------|------|
| `koda-core` | Engine library (zero terminal deps) |
| `koda-cli` | CLI binary + TUI |
| `koda-ast` | Tree-sitter AST analysis |
| `koda-email` | Email via IMAP/SMTP |

See [docs/design.md](docs/design.md) for principles and architecture.
See [CLAUDE.md](CLAUDE.md) for workspace layout and developer reference.

## Development Commands

```bash
cargo test --workspace --features koda-core/test-support  # All tests
cargo fmt --all --check                                    # Format check
cargo clippy --workspace -- -D warnings                    # Lint
cargo doc --workspace --no-deps                            # Build docs
```

## Documentation Rules

- User-facing feature added/changed → update root README + relevant crate README
- Tool added/changed in koda-ast/koda-email → update the crate README
- Architecture or design decision → add to the appropriate section in `docs/design.md`
- New crate → must ship with a README.md
- Internal refactors don't require doc updates unless they change crate
  boundaries or public APIs

## On Release

- Move CHANGELOG.md `[Unreleased]` to versioned section
- Bump version in all 4 crate Cargo.toml files
- Verify README quick-start examples still work

## Conventions

- Error handling: `anyhow::Result<T>` with `.context()`
- All I/O is async (`tokio`)
- Tool names: PascalCase; module names: snake_case
- `koda-core` has zero terminal deps (no crossterm, no ratatui)
- Cohesion over line count: don't split a file just because it's long.
  Split when pieces have genuinely independent responsibilities.

## Reporting Issues

The easiest contribution is
[reporting a bug](https://github.com/lijunzh/koda/issues/new).

## License

[MIT](LICENSE)
