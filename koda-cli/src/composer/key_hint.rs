//! Reduced port of codex's `codex-rs/tui/src/key_hint.rs`.
//!
//! ## Provenance
//!
//! Selected helpers ported from `codex-rs/tui/src/key_hint.rs` at
//! upstream commit `7e8594fc198615068018b198ab86a9ae0a541dff`.
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` for the full license text.
//!
//! ## What was kept vs dropped
//!
//! - **Kept:** [`is_altgr`]. This is the only key-hint helper used by
//!   the ported [`super::textarea::TextArea`].
//! - **Dropped:** `KeyBinding`, the modifier-prefix constants, the
//!   `From<KeyBinding> for Span` impl. Those are part of codex's hint
//!   rendering surface (footer, key-binding cheat-sheets) which koda
//!   doesn't currently use. Easy to port later if we adopt codex's
//!   hint UI.

use crossterm::event::KeyModifiers;

/// On Windows the AltGr key is delivered as `CONTROL + ALT`. The
/// textarea uses this to distinguish "user typed an AltGr-composed
/// character" (a normal text-insert) from "user pressed Ctrl+Alt+key"
/// (a chord that should be interpreted as a command).
///
/// Unconditionally returns `false` on non-Windows platforms because
/// no other OS encodes AltGr this way; on Linux/macOS the keyboard
/// driver delivers the composed character directly.
#[cfg(windows)]
#[inline]
pub(crate) fn is_altgr(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::ALT) && mods.contains(KeyModifiers::CONTROL)
}

#[cfg(not(windows))]
#[inline]
pub(crate) fn is_altgr(_mods: KeyModifiers) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_altgr_is_false_on_non_windows() {
        // On non-Windows platforms (the CI matrix here), AltGr detection
        // is a constant `false` regardless of input. Verify the
        // implementation doesn't accidentally claim true for the
        // Ctrl+Alt combo, which would break Ctrl+Alt+key chords.
        #[cfg(not(windows))]
        {
            assert!(!is_altgr(KeyModifiers::ALT | KeyModifiers::CONTROL));
            assert!(!is_altgr(KeyModifiers::NONE));
            assert!(!is_altgr(KeyModifiers::ALT));
        }

        // On Windows, Ctrl+Alt IS the AltGr signal. Verify the
        // detection returns true so the textarea treats AltGr-composed
        // characters as text input, not as a chord.
        #[cfg(windows)]
        {
            assert!(is_altgr(KeyModifiers::ALT | KeyModifiers::CONTROL));
            assert!(!is_altgr(KeyModifiers::ALT));
            assert!(!is_altgr(KeyModifiers::CONTROL));
            assert!(!is_altgr(KeyModifiers::NONE));
        }
    }
}
