//! Terminal rendering mode selection.
//!
//! Koda supports two TUI rendering strategies; this module owns the
//! enum and the env-var contract that picks between them.
//!
//! ## Modes
//!
//! - [`RenderMode::Altscreen`]: the historical default. Enters the
//!   terminal's *alternate screen buffer*, hides scrollback for the
//!   duration of the session, captures mouse for our custom selection
//!   widget, and renders into a `Viewport::Fullscreen`. On exit the
//!   alternate screen is dropped and the user's previous shell
//!   contents reappear.
//!
//! - [`RenderMode::Inline`]: the new default starting with the v0.4.x
//!   migration tracked by epic #1146. Stays in the *primary* screen,
//!   renders into a `Viewport::Inline(N)` anchored to the bottom of
//!   the terminal, and lets finalized chat history live in the
//!   terminal's native scrollback (via
//!   [`crate::inline_history::push_history`]). Mouse capture is *not*
//!   enabled, so native terminal selection / search work for free.
//!
//! ## Selection contract
//!
//! At process start [`RenderMode::from_env`] inspects `KODA_RENDER`:
//!
//! | Value (case-insensitive) | Mode |
//! |---|---|
//! | `inline` | [`RenderMode::Inline`] |
//! | `altscreen` / `alt-screen` / `alt_screen` / `fullscreen` | [`RenderMode::Altscreen`] |
//! | unset, empty, or anything else | [`RenderMode::default()`] |
//!
//! The default tracks the migration: it is **`Altscreen`** during
//! Phase A/B of #1146 (so the env var is opt-in for testers), and
//! flips to **`Inline`** at Phase D cutover. Don't read the default
//! from random places — always go through [`RenderMode::default`] so
//! the flip is one-line.

use std::env;

/// Selects which TUI rendering strategy koda uses for this session.
///
/// Defaults to [`RenderMode::Altscreen`] for now; will flip to
/// [`RenderMode::Inline`] when epic #1146 reaches Phase D.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RenderMode {
    /// Enter the terminal's alternate screen buffer; render into
    /// `Viewport::Fullscreen`. Captures mouse for in-app selection.
    Altscreen,
    /// Stay in the primary screen; render into a small bottom-anchored
    /// `Viewport::Inline(N)`; finalized history lives in native
    /// scrollback. Does not capture mouse.
    Inline,
}

impl Default for RenderMode {
    // Manual impl (vs `#[derive(Default)]` + `#[default]`) so the
    // migration intent has a place to live and be grep-able. Phase D
    // of #1146 will flip this single line; reviewers should look
    // here when bisecting render-mode regressions.
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        // Phase A/B of #1146 keeps the historical default. The flip
        // to `Inline` happens in a single commit at Phase D so it's
        // easy to bisect.
        Self::Altscreen
    }
}

impl RenderMode {
    /// Read the `KODA_RENDER` env var and resolve to a render mode.
    ///
    /// Falls back to [`RenderMode::default`] when unset, empty, or
    /// holding an unrecognized value (we don't error — the worst
    /// case is the user gets the historical UI, which is benign).
    pub(crate) fn from_env() -> Self {
        match env::var("KODA_RENDER") {
            Ok(raw) => Self::parse(&raw),
            Err(_) => Self::default(),
        }
    }

    /// Pure parser — separated from env access for testability.
    fn parse(raw: &str) -> Self {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "inline" => Self::Inline,
            "altscreen" | "alt-screen" | "alt_screen" | "fullscreen" => Self::Altscreen,
            // Unknown / empty: don't surprise the user with a mode
            // they didn't ask for. Falling back to default also
            // means future modes can be added without breaking
            // sessions that hard-coded an unknown value.
            _ => Self::default(),
        }
    }

    /// Convenience predicate for the inline branch.
    pub(crate) fn is_inline(self) -> bool {
        matches!(self, Self::Inline)
    }
}

#[cfg(test)]
mod tests {
    //! Tests cover the env contract end-to-end. We avoid touching the
    //! real `KODA_RENDER` env var (parallel test runs would race) by
    //! exercising `parse` directly; one test wraps `from_env` with
    //! an explicit `KODA_RENDER` set/clear to confirm the env path.

    use super::*;
    use serial_test::serial;

    #[test]
    fn default_is_altscreen_during_migration_phase_ab() {
        // Sanity check that the migration default hasn't accidentally
        // flipped before Phase D. When the cutover happens this test
        // *should* be updated to assert `Inline` — that update is the
        // signal that Phase D shipped.
        assert_eq!(RenderMode::default(), RenderMode::Altscreen);
    }

    #[test]
    fn parse_inline_variants() {
        assert_eq!(RenderMode::parse("inline"), RenderMode::Inline);
        assert_eq!(RenderMode::parse("INLINE"), RenderMode::Inline);
        assert_eq!(RenderMode::parse("  Inline  "), RenderMode::Inline);
    }

    #[test]
    fn parse_altscreen_aliases() {
        // We accept several spellings because users will guess.
        // Accepting them is cheap and friendlier than erroring.
        for value in ["altscreen", "alt-screen", "alt_screen", "fullscreen", "ALTSCREEN"] {
            assert_eq!(
                RenderMode::parse(value),
                RenderMode::Altscreen,
                "value {value:?} should parse as Altscreen",
            );
        }
    }

    #[test]
    fn parse_unknown_falls_back_to_default() {
        // Garbage values must not panic and must yield a sensible
        // session. Any unrecognized string falls through to default
        // (currently Altscreen).
        for value in ["", "   ", "yes", "true", "0", "splat", "auto"] {
            assert_eq!(
                RenderMode::parse(value),
                RenderMode::default(),
                "value {value:?} should fall back to default",
            );
        }
    }

    #[test]
    fn is_inline_predicate() {
        assert!(RenderMode::Inline.is_inline());
        assert!(!RenderMode::Altscreen.is_inline());
    }

    #[test]
    #[serial] // mutates global env; serialize to avoid races with other env-touching tests
    fn from_env_reads_koda_render() {
        // SAFETY: `set_var`/`remove_var` are unsafe in Rust 2024 because
        // they race with other threads reading env. The `serial` attr
        // serializes this against other env-touching tests; within this
        // process there are no other readers during the assertion.
        // Save and restore the previous value so we don't pollute other
        // tests' environment.
        let prev = env::var("KODA_RENDER").ok();
        unsafe { env::set_var("KODA_RENDER", "inline") };
        assert_eq!(RenderMode::from_env(), RenderMode::Inline);

        unsafe { env::set_var("KODA_RENDER", "altscreen") };
        assert_eq!(RenderMode::from_env(), RenderMode::Altscreen);

        unsafe { env::remove_var("KODA_RENDER") };
        assert_eq!(RenderMode::from_env(), RenderMode::default());

        // Restore prior value so test ordering doesn't matter.
        match prev {
            Some(v) => unsafe { env::set_var("KODA_RENDER", v) },
            None => unsafe { env::remove_var("KODA_RENDER") },
        }
    }
}
