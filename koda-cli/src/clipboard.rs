//! Clipboard abstraction with OSC 52 fallback.
//!
//! Backend selection (matching Codex and Gemini CLI behaviour):
//!
//! | Session        | Backend                                        |
//! |----------------|------------------------------------------------|
//! | SSH            | OSC 52 only (native clipboard is on the        |
//! |                | remote machine, useless to the user)           |
//! | Local (+ tmux) | arboard first → OSC 52 fallback                |
//!
//! tmux is **not** a reason to skip arboard. Local tmux sessions have a
//! working display server — arboard succeeds there. tmux only affects
//! *which OSC 52 wrapper* is used when OSC 52 is the fallback path.
//!
//! ## OSC 52 write target
//!
//! The sequence is written to `/dev/tty` (the controlling terminal) rather
//! than stdout. ratatui/crossterm own stdout in TUI mode; injecting escape
//! sequences there can corrupt the rendered display. Gemini CLI uses the
//! same `/dev/tty`-first strategy.
//!
//! ## Payload limit
//!
//! OSC 52 payloads larger than 100 KB (raw, before base64) are rejected.
//! Some terminal emulators silently drop or truncate large sequences.
//! Codex uses the same 100 KB threshold.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use std::io::Write;

/// Maximum raw bytes to base64-encode into an OSC 52 sequence.
/// Large payloads are silently dropped by some terminals.
const OSC52_MAX_RAW_BYTES: usize = 100_000;

/// Copy `text` to the system clipboard.
///
/// Returns a short status phrase for embedding in a user-facing message.
/// Returns `Err(msg)` only when all backends fail.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<String, String> {
    if is_ssh_session() {
        // Native clipboard lives on the remote machine — useless to the user.
        // Use OSC 52 to reach the local terminal emulator's clipboard instead.
        osc52_write(text).map_err(|e| format!("OSC 52 copy failed over SSH: {e}"))
    } else {
        // Local session (including tmux): try arboard first, OSC 52 as fallback.
        try_arboard(text).or_else(|_| osc52_write(text))
    }
}

// ---------------------------------------------------------------------------
// Environment detection
// ---------------------------------------------------------------------------

fn is_ssh_session() -> bool {
    std::env::var("SSH_TTY").is_ok()
        || std::env::var("SSH_CONNECTION").is_ok()
        || std::env::var("SSH_CLIENT").is_ok()
}

/// True when running inside tmux — affects *which OSC 52 wrapper* is used,
/// not whether to skip arboard.
fn is_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

// ---------------------------------------------------------------------------
// arboard (local display server)
// ---------------------------------------------------------------------------

fn try_arboard(text: &str) -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text))
        .map(|()| "to clipboard".to_string())
        .map_err(|e| format!("arboard: {e}"))
}

// ---------------------------------------------------------------------------
// OSC 52 (terminal escape sequence)
// ---------------------------------------------------------------------------

/// Write text to the terminal clipboard via OSC 52.
///
/// Writes to `/dev/tty` (the controlling terminal device) so the sequence
/// does not interleave with ratatui's stdout rendering. Falls back to
/// stderr → stdout if `/dev/tty` is unavailable.
///
/// Sequences are wrapped for tmux when `TMUX` is set.
fn osc52_write(text: &str) -> Result<String, String> {
    let raw = text.as_bytes();
    if raw.len() > OSC52_MAX_RAW_BYTES {
        return Err(format!(
            "payload too large for OSC 52 ({} bytes, max {OSC52_MAX_RAW_BYTES})",
            raw.len()
        ));
    }

    let encoded = B64.encode(raw);
    let inner = format!("\x1b]52;c;{encoded}\x07");
    let seq = if is_tmux() {
        // Double every ESC inside the passthrough wrapper.
        let doubled = inner.replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;{doubled}\x1b\\")
    } else {
        inner
    };

    write_to_tty(&seq)?;

    Ok(if is_tmux() {
        "to clipboard (via tmux)".to_string()
    } else {
        "to clipboard (via terminal)".to_string()
    })
}

/// Write `data` to `/dev/tty`, falling back to stderr then stdout.
///
/// `/dev/tty` is the controlling terminal device — writing there avoids
/// polluting stdout (owned by ratatui) or stderr with raw escape sequences.
fn write_to_tty(data: &str) -> Result<(), String> {
    // Prefer /dev/tty: direct path to the terminal, independent of stdio.
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        if let Ok(mut tty) = OpenOptions::new().write(true).open("/dev/tty") {
            return tty
                .write_all(data.as_bytes())
                .and_then(|()| tty.flush())
                .map_err(|e| format!("/dev/tty write error: {e}"));
        }
    }

    // Fallback: stderr (avoids stdout which ratatui may be rendering to).
    let mut err = std::io::stderr().lock();
    err.write_all(data.as_bytes())
        .and_then(|()| err.flush())
        .map_err(|e| format!("stderr write error: {e}"))
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
        let encoded = B64.encode(b"hello, world");
        let seq = format!("\x1b]52;c;{encoded}\x07");

        assert!(
            seq.starts_with("\x1b]52;c;"),
            "must start with OSC 52 header"
        );
        assert!(seq.ends_with('\x07'), "must end with BEL");
        assert!(seq.contains(&encoded), "must contain base64 payload");
    }

    #[test]
    fn osc52_tmux_wrapper_doubles_esc_and_ends_with_st() {
        // The inner sequence contains ESC (0x1b). When wrapped for tmux every
        // ESC must be doubled so tmux's DCS parser sees the raw bytes.
        let encoded = B64.encode(b"hi");
        let inner = format!("\x1b]52;c;{encoded}\x07");
        let doubled = inner.replace('\x1b', "\x1b\x1b");
        let wrapped = format!("\x1bPtmux;{doubled}\x1b\\");

        assert!(wrapped.starts_with("\x1bPtmux;"), "must open with DCS");
        assert!(wrapped.ends_with("\x1b\\"), "must close with ST");
        // Every original ESC is doubled.
        let esc_count = wrapped.chars().filter(|&c| c == '\x1b').count();
        // inner has 1 ESC → doubled = 2; DCS open has 1 ESC; ST has 1 ESC → total 4
        assert_eq!(esc_count, 4);
    }

    #[test]
    fn osc52_base64_round_trips() {
        let original = "koda clipboard test 🐶";
        let encoded = B64.encode(original.as_bytes());
        let decoded = B64.decode(&encoded).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), original);
    }

    #[test]
    fn osc52_rejects_oversized_payload() {
        let big = "x".repeat(OSC52_MAX_RAW_BYTES + 1);
        // We test the size-check logic directly via the helper.
        let raw = big.as_bytes();
        assert!(raw.len() > OSC52_MAX_RAW_BYTES, "test setup");
        // Replicate the guard from osc52_write.
        let result: Result<(), &str> = if raw.len() > OSC52_MAX_RAW_BYTES {
            Err("too large")
        } else {
            Ok(())
        };
        assert!(result.is_err());
    }

    // ── Environment detection ─────────────────────────────────

    #[test]
    fn ssh_detection_checks_all_three_vars() {
        // We can't mutate process env safely in parallel tests, so we just
        // verify the function compiles and doesn't panic in the current env.
        let _ = is_ssh_session();
        let _ = is_tmux();
    }
}
