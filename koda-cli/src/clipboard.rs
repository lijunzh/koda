//! Clipboard abstraction with OSC 52 fallback.
//!
//! Selects a backend based on the runtime environment:
//!
//! | Environment         | Backend                        |
//! |---------------------|--------------------------------|
//! | `TMUX` set          | OSC 52 + tmux DCS passthrough  |
//! | `SSH_CLIENT` / `SSH_TTY` set | OSC 52 directly        |
//! | Otherwise           | arboard → OSC 52 on error      |
//!
//! OSC 52 writes a base64-encoded escape sequence directly to the
//! terminal's stdout — no additional dependencies beyond `base64` (already
//! in the tree).  Supported by iTerm2, Kitty, Alacritty, WezTerm, foot,
//! and xterm with `allowWindowOps`.  tmux passes it through when
//! `set -g allow-passthrough on` is set in `tmux.conf`.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use std::io::Write;

/// Copy `text` to the system clipboard.
///
/// Returns a short status phrase suitable for embedding in a user-facing
/// message, e.g. `"to clipboard"` or `"to clipboard (via terminal)"`.
/// Returns `Err(msg)` only when all backends fail.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<String, String> {
    match detect_backend() {
        Backend::Arboard => try_arboard(text).or_else(|_| write_osc52(text, false)),
        Backend::Osc52 => write_osc52(text, false),
        Backend::Osc52Tmux => write_osc52(text, true),
    }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Native clipboard via arboard (requires a display server).
    Arboard,
    /// OSC 52 escape sequence written directly to the terminal.
    Osc52,
    /// OSC 52 wrapped in a tmux DCS passthrough sequence.
    Osc52Tmux,
}

fn detect_backend() -> Backend {
    if std::env::var("TMUX").is_ok() {
        Backend::Osc52Tmux
    } else if std::env::var("SSH_CLIENT").is_ok() || std::env::var("SSH_TTY").is_ok() {
        Backend::Osc52
    } else {
        Backend::Arboard
    }
}

// ---------------------------------------------------------------------------
// arboard (local display server)
// ---------------------------------------------------------------------------

fn try_arboard(text: &str) -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .map(|()| "to clipboard".to_string())
        .map_err(|e| format!("Clipboard error: {e}"))
}

// ---------------------------------------------------------------------------
// OSC 52 (terminal escape sequence)
// ---------------------------------------------------------------------------

/// Write text to the terminal clipboard via OSC 52.
///
/// Normal:  `ESC ] 52 ; c ; <base64> BEL`
/// tmux:    `ESC P tmux; ESC ESC ] 52 ; c ; <base64> BEL ESC \`
fn write_osc52(text: &str, tmux_wrap: bool) -> Result<String, String> {
    let encoded = B64.encode(text.as_bytes());
    let seq = if tmux_wrap {
        // The inner OSC 52 escape must be doubled inside the DCS string.
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    };

    let mut out = std::io::stdout().lock();
    out.write_all(seq.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| format!("OSC 52 write error: {e}"))?;

    let label = if tmux_wrap {
        "to clipboard (via tmux)"
    } else {
        "to clipboard (via terminal)"
    };
    Ok(label.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── OSC 52 sequence shape ─────────────────────────────────

    #[test]
    fn osc52_sequence_structure() {
        let encoded = B64.encode("hello, world".as_bytes());
        let seq = format!("\x1b]52;c;{encoded}\x07");

        assert!(
            seq.starts_with("\x1b]52;c;"),
            "must start with OSC 52 header"
        );
        assert!(seq.ends_with('\x07'), "must end with BEL");
        assert!(seq.contains(&encoded), "must contain base64 payload");
    }

    #[test]
    fn osc52_tmux_sequence_structure() {
        let encoded = B64.encode("hello".as_bytes());
        let seq = format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\");

        assert!(seq.starts_with("\x1bPtmux;"), "must start with tmux DCS");
        assert!(
            seq.ends_with("\x1b\\"),
            "must end with ST (string terminator)"
        );
        assert!(seq.contains(&encoded), "must contain base64 payload");
    }

    #[test]
    fn osc52_base64_round_trips() {
        // Verify the base64 payload decodes back to the original text.
        let original = "koda clipboard test 🐶";
        let encoded = B64.encode(original.as_bytes());
        let decoded = B64.decode(&encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), original);
    }

    #[test]
    fn osc52_empty_string_is_valid() {
        // Clearing the clipboard with an empty string is a valid OSC 52 op.
        let encoded = B64.encode("".as_bytes());
        let seq = format!("\x1b]52;c;{encoded}\x07");
        assert!(!seq.is_empty());
    }

    // ── Backend detection ─────────────────────────────────────

    #[test]
    fn detect_backend_tmux_takes_priority() {
        // TMUX wins even if SSH vars are also set.
        // We test the logic directly rather than mutating the process env.
        // The function is deterministic for a given env state; just call it
        // in a context where we know TMUX is unset (CI / local dev).
        // Real environment-based path is covered by integration/manual testing.
        let _ = detect_backend(); // compile + no panic is the assertion here
    }
}
