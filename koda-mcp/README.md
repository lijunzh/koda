# koda-mcp

The MCP client of [Koda](https://github.com/lijunzh/koda), the AI coding agent: connecting to MCP servers over stdio and HTTP and bridging their tools.

Servers are configured as in the manual's [MCP servers](https://lijunzh.github.io/koda/mcp.html) chapter.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly. To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client).

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-mcp)

## License

MIT
