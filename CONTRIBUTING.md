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

The hook runs:
1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`

That's it — typical wall-clock is ~6–35s depending on cache warmth. CI runs
everything else (`cargo check`, full test matrix on Linux + macOS, `cargo doc`,
`cargo audit`).

> **`--no-verify` is for genuine WIP pushes only** (draft PR, CI debugging,
> mid-refactor checkpoint). Using it on a branch you intend to merge defeats
> the hook entirely — CI will catch what you skipped, usually at the worst
> time.

## Reporting Issues

The easiest contribution is
[reporting a bug](https://github.com/lijunzh/koda/issues/new).

## License

[MIT](LICENSE)
