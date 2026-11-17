# koda-git

Git for [Koda](https://github.com/lijunzh/koda), the AI coding agent: one hardened way to run `git` for the commands Koda runs itself (the git context at session start, sub-agent worktrees, `/diff`), so a repository's own configuration can't make git run programs: hooks, an fsmonitor, filters, an external diff or a textconv. The settings are Codex's.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-git)

## License

MIT
