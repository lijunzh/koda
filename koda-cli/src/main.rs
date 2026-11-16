//! Koda 🐻 — binary entry point.
//!
//! All logic lives in [`koda_cli::run()`]. This file is intentionally thin
//! so that `koda-cli` can be tested as a library without pulling in the
//! binary's argument parsing context.

fn main() -> anyhow::Result<()> {
    // First, before any thread starts: `koda` also runs the sandbox's
    // helpers, as Codex's binary does, so every install has them.
    koda_sandbox::dispatch_helpers();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(koda_cli::run())
}
