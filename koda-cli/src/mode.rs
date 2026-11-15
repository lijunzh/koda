//! The session's mode as the TUI keeps it: the protocol's [`TrustMode`],
//! as the host reports it, and the host's sandbox from `state/read`. The
//! host enforces the rules (Auto needs the sandbox, an interactive session
//! runs in Safe rather than Plan); these checks only spare a round trip and
//! say why, as Codex's TUI keeps its thread's policy as the server sent it.

use koda_client::protocol::state::{SandboxState, TrustMode};

/// What Shift+Tab switches to: Safe and Auto in turn. Without the host's
/// sandbox, Auto is refused and the error says what to do.
pub(crate) fn next(mode: TrustMode, sandbox: &SandboxState) -> Result<TrustMode, String> {
    allow(
        match mode {
            TrustMode::Safe => TrustMode::Auto,
            TrustMode::Auto => TrustMode::Safe,
            // Not a mode an interactive session runs in.
            TrustMode::Plan => TrustMode::Safe,
        },
        sandbox,
    )
}

/// `mode`, unless it's Auto and the host has no sandbox.
pub(crate) fn allow(mode: TrustMode, sandbox: &SandboxState) -> Result<TrustMode, String> {
    if mode == TrustMode::Auto && !sandbox.available {
        Err(auto_refused(sandbox))
    } else {
        Ok(mode)
    }
}

/// Why Auto isn't available, and what to do about it.
pub(crate) fn auto_refused(sandbox: &SandboxState) -> String {
    let why = sandbox
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    let mut message = format!(
        "Auto mode requires the kernel sandbox, which is unavailable on this system{why}. \
         Staying in Safe mode, which asks before anything with a side effect."
    );
    if let Some(hint) = &sandbox.setup_hint {
        message.push_str("\n\nSetup:\n");
        message.push_str(hint);
    }
    message
}

/// The mode's name in the status bar: `plan`, `safe` or `auto`.
pub(crate) fn label(mode: TrustMode) -> &'static str {
    match mode {
        TrustMode::Plan => "plan",
        TrustMode::Safe => "safe",
        TrustMode::Auto => "auto",
    }
}

/// The mode's icon in the status bar and the composer's prompt.
pub(crate) fn icon(mode: TrustMode) -> &'static str {
    match mode {
        TrustMode::Plan => "\u{1f4cb}",
        TrustMode::Safe => "\u{1f512}",
        TrustMode::Auto => "\u{26a1}",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(available: bool) -> SandboxState {
        SandboxState {
            backend: "bwrap".to_string(),
            available,
            reason: (!available).then(|| "bwrap isn't installed".to_string()),
            setup_hint: (!available).then(|| "Install bubblewrap".to_string()),
        }
    }

    #[test]
    fn shift_tab_cycles_safe_and_auto() {
        assert_eq!(next(TrustMode::Safe, &sandbox(true)), Ok(TrustMode::Auto));
        assert_eq!(next(TrustMode::Auto, &sandbox(true)), Ok(TrustMode::Safe));
        assert_eq!(next(TrustMode::Plan, &sandbox(true)), Ok(TrustMode::Safe));
    }

    #[test]
    fn auto_without_the_sandbox_says_what_to_do() {
        let refused = next(TrustMode::Safe, &sandbox(false)).unwrap_err();
        assert!(refused.contains("bwrap isn't installed"), "{refused}");
        assert!(refused.contains("Install bubblewrap"), "{refused}");
        assert_eq!(next(TrustMode::Auto, &sandbox(false)), Ok(TrustMode::Safe));
    }
}
