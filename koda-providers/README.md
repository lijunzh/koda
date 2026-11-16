# koda-providers

Model providers for [Koda](https://github.com/lijunzh/koda), the AI coding agent: Anthropic, Gemini and OpenAI-compatible clients, and the model tables.

Providers and model aliases are documented in the manual's [providers](https://lijunzh.github.io/koda/providers.html) chapter.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-providers)

## License

MIT
