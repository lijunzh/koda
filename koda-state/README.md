# koda-state

The stored state of [Koda](https://github.com/lijunzh/koda), the AI coding agent: its sessions, SQLite files and diagnostic log database.

Where data lives, and how to delete it: the manual's [privacy & data](https://lijunzh.github.io/koda/privacy.html) page.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-state)

## License

MIT
