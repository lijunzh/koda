# koda-oss

Local models for [Koda](https://github.com/lijunzh/koda), the AI coding agent: `koda --oss` starts llama.cpp's `llama-server` with a model sized for the machine, shares it between Koda processes, and stops it when the last one exits.

See the manual's [Local models](https://lijunzh.github.io/koda/local-models.html#one-command-koda---oss).

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-oss)

## License

MIT
