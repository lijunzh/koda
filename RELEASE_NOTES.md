# Koda v0.1.5 — Simplify & Polish

This release strips away complexity added in v0.1.4 (phase system, tier system, OPAR)
and replaces it with a simple, direct inference loop. The model drives execution:
stream response → execute tool calls → repeat.

## Highlights

- **Simplified architecture** — removed the six-phase state machine, three-tier model
  system, and OPAR remnants. Fewer abstractions, faster iteration, easier to reason about.
- **TUI improvements** — cleaner status bar (no more tier label), correct approval mode
  colors, model name truncation, proper Unicode width handling, narrow terminal guard.
- **ratatui 0.30** — major dependency upgrade with ratatui-textarea 0.8 and crossterm 0.29.
- **User guide** — new `docs/user-guide.md` with comprehensive workflow documentation.
- **Doc freshness CI** — automated tests verify capabilities.md and user guide stay
  in sync with the codebase.

## What's Changed

| Category | Summary |
|----------|---------|
| Architecture | Phase system, tier system, OPAR removed (#354, #355, #357) |
| TUI | Status bar polish, mode colors, narrow terminal guard (#380) |
| Docs | User guide, DESIGN.md cleanup, capabilities refresh (#299, #301, #378, #379) |
| Security | quinn-proto 0.11.14 (RUSTSEC-2026-0037) (#393) |
| Dependencies | ratatui 0.30, crossterm 0.29, tree-sitter-go 0.25, which 8.0, mail-parser 0.11 |
| Tests | 671 tests (up from 432 in v0.1.4) |

## Breaking Changes

None. The phase/tier systems were internal — no user-facing API changes.

## Known Issues

- `rsa` 0.9.10 has a medium-severity timing sidechannel (RUSTSEC-2023-0071). No fix
  available; transitive dependency via sqlx-mysql. Not exploitable in koda's usage.
- `bincode` 1.3.3 is unmaintained (RUSTSEC-2025-0141). Transitive via syntect.
  No action required until syntect migrates.
