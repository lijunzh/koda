# Changelog

All notable changes to Koda are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

> **Lineage:** This project continues from [`koda-agent`](https://github.com/lijunzh/koda-agent) (archived).
> Versions v0.1.0–v0.1.5 of `koda-agent` are documented in that repository's CHANGELOG.

## [Unreleased]

First release of `koda-core` and `koda-cli` as separate crates.

### Architecture
- **Workspace split**: Single `koda-agent` crate → `koda-core` (library) + `koda-cli` (binary)
  - `koda-core`: pure engine with zero terminal dependencies
  - `koda-cli`: CLI frontend, produces the `koda` binary
  - `cargo install koda-cli` replaces `cargo install koda-agent`
- **Channel-based approval**: Approval flows through async `EngineEvent::ApprovalRequest` + `EngineCommand::ApprovalResponse` over `tokio::mpsc` channels — works over any transport
- **CancellationToken**: Replaces global `AtomicBool` interrupt flag. Proper per-session cancellation
- **KodaAgent**: Shared, immutable agent resources (tools, system prompt, MCP registry). `Arc`-shareable for parallel sub-agents
- **KodaSession**: Per-conversation state (DB, provider, settings, cancel token). `run_turn()` replaces 15-parameter `inference_loop()` call

### Still TODO before v0.1.0
- [ ] Phase 4: ACP server (`koda server` subcommand)
- [ ] Phase 5: Remote CLI client (`koda connect`)
- [ ] Fix `version.rs` to check `koda-cli` on crates.io
- [ ] Update homebrew formula test assertion
- [ ] CI/CD: verify dual crate publishing pipeline

### Testing
- 347 tests across `koda-core/tests/` and `koda-cli/tests/`
- All CI checks passing: cargo fmt, clippy -D warnings, test, doc
