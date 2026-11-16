# koda-server

The host of [Koda](https://github.com/lijunzh/koda), the AI coding agent: the server end of its client protocol.

It runs sessions, settings and MCP behind the [client protocol](https://lijunzh.github.io/koda/serve.html), driving the engine (`koda-core`). Programs reach it through `koda-client`, or as `koda serve --stdio`.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-server)

## License

MIT
