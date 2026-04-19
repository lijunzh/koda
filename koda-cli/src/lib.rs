//! # koda-cli
//!
//! Internal library crate for the `koda` binary.
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

pub(crate) mod acp_adapter;
pub(crate) mod ansi_parse;
pub(crate) mod app;
pub(crate) mod builtin_skills;
pub(crate) mod clipboard;
pub(crate) mod completer;
pub(crate) mod diff_render;
pub(crate) mod headless;
pub(crate) mod highlight;
pub(crate) mod history_render;
pub(crate) mod input;
pub(crate) mod md_render;
pub(crate) mod mouse_select;
pub(crate) mod onboarding;
pub(crate) mod queue_lanes;
pub(crate) mod repl;
pub(crate) mod scroll_buffer;
pub(crate) mod server;
pub(crate) mod sink;
pub(crate) mod startup;
pub(crate) mod theme;
pub(crate) mod tool_header;
pub(crate) mod tool_history;
pub(crate) mod tool_output_lines;
pub(crate) mod transcript;
pub(crate) mod tui_app;
pub(crate) mod tui_commands;
pub(crate) mod tui_context;
pub(crate) mod tui_handlers_inference;
pub(crate) mod tui_mcp;
pub(crate) mod tui_output;
pub(crate) mod tui_render;
pub(crate) mod tui_types;
pub(crate) mod tui_viewport;
pub(crate) mod tui_wizards;
pub(crate) mod util;
pub(crate) mod widgets;
pub(crate) mod wrap_input;
pub(crate) mod wrap_util;

/// Binary entry point. The only public symbol the `koda` binary needs.
pub async fn run() -> anyhow::Result<()> {
    app::run().await
}
