# koda-client

The client API of [Koda](https://github.com/lijunzh/koda), the AI coding agent: start Koda, send requests, read its answers.

**This is the crate to embed Koda with.** The TUI, `koda exec` and `koda serve` use it: `InProcessClient` starts Koda in-process, sends typed requests and reads the [client protocol](https://lijunzh.github.io/koda/serve.html)'s events.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly.

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-client)

## License

MIT
