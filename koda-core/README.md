# koda-core

The engine of [Koda](https://github.com/lijunzh/koda), the AI coding agent:
the agent loop, the built-in tools, approval and trust modes, compaction and
sub-agents.

**To embed Koda, use [`koda-client`](https://crates.io/crates/koda-client)**,
the client API the TUI, `koda exec` and `koda serve` use: it starts Koda
in-process, sends typed requests and reads the client protocol's events. The
engine sits behind it, driven by `koda-server` through `EngineCommand` and
`EngineEvent`.

**Versioning:** published so that `cargo install koda-cli` builds. It follows
koda-cli's version, and its Rust API makes no semver promise of its own: a
0.7.x release may change it. The Koda crates pin each other exactly.

## What's inside

- **The agent loop:** streaming inference with tool execution, parallel where
  it's safe.
- **Tools:** read (text, images, PDFs, notebooks), search, edit, shell, web,
  memory, todo and sub-agents.
- **Approval:** the trust modes (Auto, Safe, Plan) and each tool call's
  effect, with the kernel sandbox from `koda-sandbox`.
- **Compaction:** summarizing old messages when the context fills.

Model providers live in `koda-providers` (re-exported here as `providers`),
stored sessions in `koda-state`, the MCP client in `koda-mcp`, and instruction
files and skills in `koda-memory` and `koda-skills`.

- [User manual](https://lijunzh.github.io/koda/)
- [API docs](https://docs.rs/koda-core)
- [Design](https://github.com/lijunzh/koda/blob/main/DESIGN.md)

## License

MIT
