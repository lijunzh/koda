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

Activate the pre-push hook to catch `fmt`, `clippy`, and test failures before
they hit CI:

```bash
git config core.hooksPath .githooks
```

The hook runs:
1. `cargo fmt --all --check`
2. `cargo clippy --workspace … -D warnings`
3. `cargo check --workspace --all-targets`
4. `cargo test --lib` across the workspace + all fast integration suites
   (`e2e_safety_test`, `e2e_tools_test`, `file_tools_test`, `guarantee_matrix_test`, …)

Slow suites (`mcp_http_transport`, `inference_edge`, `inference_recovery`) are
skipped in the hook — they involve sleep/retry loops and would make every push
take 5+ minutes. CI runs them on every push.

> **`--no-verify` is for genuine WIP pushes only** (draft PR, CI debugging,
> mid-refactor checkpoint). Using it on a branch you intend to merge defeats
> the hook entirely — CI will catch what you skipped, usually at the worst
> time.

## Reporting Issues

The easiest contribution is
[reporting a bug](https://github.com/lijunzh/koda/issues/new).

## License

[MIT](LICENSE)
