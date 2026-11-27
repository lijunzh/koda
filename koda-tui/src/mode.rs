//! The session's mode as the TUI keeps it: the protocol's [`TrustMode`],
//! as the host reports it, as Codex's TUI keeps its thread's policy as the
//! server sent it.

use koda_client::protocol::state::TrustMode;

/// What Shift+Tab switches to: Auto and Plan in turn, as Claude Code's
/// Shift+Tab reaches its plan mode.
pub(crate) fn next(mode: TrustMode) -> TrustMode {
    mode.next()
}

/// The mode's name in the status bar: `plan` or `auto`.
pub(crate) fn label(mode: TrustMode) -> &'static str {
    match mode {
        TrustMode::Plan => "plan",
        TrustMode::Auto => "auto",
    }
}

/// The mode's icon in the status bar and the composer's prompt.
pub(crate) fn icon(mode: TrustMode) -> &'static str {
    match mode {
        TrustMode::Plan => "\u{1f4cb}",
        TrustMode::Auto => "\u{26a1}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_tab_switches_between_auto_and_plan() {
        assert_eq!(next(TrustMode::Auto), TrustMode::Plan);
        assert_eq!(next(TrustMode::Plan), TrustMode::Auto);
    }
}
