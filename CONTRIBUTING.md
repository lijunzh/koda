# Contributing to Koda

All contributor guidance lives in [CLAUDE.md](CLAUDE.md) — the single source of
truth for project structure, conventions, build commands, and documentation rules.

## Quick Start

```bash
git clone https://github.com/lijunzh/koda.git
cd koda
cargo build
cargo test --workspace --features koda-core/test-support
```

## Local checks (one-time setup)

Activate the pre-push hook to catch `fmt` and `clippy` failures before they
hit CI:

```bash
git config core.hooksPath .githooks
```

The hook mirrors the Lint CI job exactly. Skip it for a WIP push with
`git push --no-verify`.

## Reporting Issues

The easiest contribution is
[reporting a bug](https://github.com/lijunzh/koda/issues/new).

## License

[MIT](LICENSE)
