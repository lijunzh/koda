//! Koda 🐻 — binary entry point.
//!
//! All logic lives in [`koda_cli::run()`]. This file is intentionally thin
//! so that `koda-cli` can be tested as a library without pulling in the
//! binary's argument parsing context.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    koda_cli::run().await
}
