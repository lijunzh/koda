# koda-protocol

The client protocol of [Koda](https://github.com/lijunzh/koda), the AI coding agent: JSON-RPC 2.0 messages and their types.

The messages are documented for users in the manual's [client protocol](https://lijunzh.github.io/koda/headless.html#the-client-protocol) section; `koda serve --stdio` and `koda exec --json` speak them.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-protocol)

## License

MIT
