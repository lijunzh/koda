//! # koda-cli
//!
//! Internal library crate for the `koda` binary: the command line, `koda
//! exec`, `koda mcp` and logging. The TUI is `koda-tui`, and `--oss` is
//! `koda-oss`.
//!
//! **User-facing documentation lives at <https://lijunzh.github.io/koda/>**
//!
//! This crate's public API surface is exactly one function:
//! [`run()`] — called by `main.rs`. Everything else is `pub(crate)`.

// koda requires Unix (macOS or Linux). The Bash tool uses `sh` which does
// not exist on Windows. Fail at compile time with a clear message.
#[cfg(not(unix))]
compile_error!(
    "koda requires a Unix-like operating system (macOS or Linux). \
     Windows is not supported. On Windows, use WSL2 instead: \
     https://learn.microsoft.com/windows/wsl"
);

pub(crate) mod app;
pub(crate) mod headless;
pub(crate) mod logging;
pub(crate) mod mcp_cmd;
pub(crate) mod onboarding;
pub(crate) mod output_schema;

/// Binary entry point. The only public symbol the `koda` binary needs.
pub async fn run() -> anyhow::Result<()> {
    app::run().await
}
