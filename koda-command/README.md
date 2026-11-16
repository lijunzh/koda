# koda-command

Shell command classification for [Koda](https://github.com/lijunzh/koda), the AI coding agent: what a command does, and the paths it touches.

Koda uses it to decide which commands need approval and which stay inside the project; see the manual's [trust modes](https://lijunzh.github.io/koda/approval.html).

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-command)

## License

MIT
