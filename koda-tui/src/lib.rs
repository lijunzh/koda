//! # koda-tui
//!
//! The terminal interface of the `koda` command: the fullscreen TUI it
//! opens. `koda-cli` parses the command line and calls [`tui_app::run`];
//! the modules it shares with `koda exec` are public too. The TUI reaches
//! the engine only through `koda-client`.
//!
//! **User-facing documentation lives at <https://lijunzh.github.io/koda/>**

// koda requires Unix (macOS or Linux). The Bash tool uses `sh` which does
// not exist on Windows. Fail at compile time with a clear message.
#[cfg(not(unix))]
compile_error!(
    "koda requires a Unix-like operating system (macOS or Linux). \
     Windows is not supported. On Windows, use WSL2 instead: \
     https://learn.microsoft.com/windows/wsl"
);

pub(crate) mod ansi_parse;
pub mod backend;
pub(crate) mod child_activity;
pub(crate) mod clipboard;
pub(crate) mod clipboard_image;
pub(crate) mod completer;
pub(crate) mod composer;
pub(crate) mod diff_render;
pub(crate) mod elicitation_form;
pub(crate) mod file_search;
pub(crate) mod frame_requester;
pub(crate) mod highlight;
pub(crate) mod hyperlink;
pub(crate) mod inference;
pub mod input;
pub(crate) mod input_history;
pub(crate) mod legacy_clones;
pub(crate) mod md_render;
pub(crate) mod mode;
pub(crate) mod mouse_select;
pub(crate) mod notifications;
pub(crate) mod panic_log;
pub mod protocol_text;
pub mod providers;
pub(crate) mod queued;
pub mod retired_env;
pub(crate) mod scroll_buffer;
pub mod session_id;
pub(crate) mod startup;
pub(crate) mod theme;
pub(crate) mod tool_header;
pub(crate) mod tool_history;
pub(crate) mod tool_output_lines;
pub(crate) mod tool_summary;
pub mod truncate;
pub mod tui_app;
pub(crate) mod tui_commands;
pub(crate) mod tui_context;
pub(crate) mod tui_output;
pub(crate) mod tui_render;
pub(crate) mod tui_types;
pub(crate) mod tui_viewport;
pub(crate) mod tui_wizards;
pub mod updates;
pub mod user_config;
pub(crate) mod util;
pub(crate) mod widgets;
pub(crate) mod wrap_util;

/// What to do before a signal ends the TUI: the exit that follows runs no
/// destructors, so `koda-cli` registers here what must not outlive it (the
/// `--oss` server).
static SIGNAL_EXIT_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Run `hook` when SIGHUP, SIGINT or SIGTERM ends the TUI, after the
/// terminal is restored and before the process exits. Only the first call
/// counts.
pub fn set_signal_exit_hook(hook: fn()) {
    let _ = SIGNAL_EXIT_HOOK.set(hook);
}

pub(crate) fn run_signal_exit_hook() {
    if let Some(hook) = SIGNAL_EXIT_HOOK.get() {
        hook();
    }
}
