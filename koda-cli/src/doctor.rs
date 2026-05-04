//! `koda doctor` subcommand — platform diagnostics for support and setup.
//!
//! Prints sandbox availability, version, OS, and trust default in a
//! plain-text format suitable for pasting into bug reports. Pure
//! read-only inspection — no side effects, no file writes, no network.
//!
//! # Why a dedicated subcommand
//!
//! Pre-`doctor`, the only signals about sandbox availability were:
//!   - A one-time `tracing::warn!` (visible only with `RUST_LOG=warn`)
//!   - A bail at first sandboxed `build()` call (per-tool, not startup)
//!
//! Neither helped users debug "why won't `--mode auto` start" or
//! "what does my system actually support". `koda doctor` is the
//! self-serve answer, linkable from the bug-report template and the
//! `Auto requires sandbox` error message itself.
//!
//! # Output stability
//!
//! The output format is **stable** — bug reports paste it verbatim,
//! and a future support tool may parse it. Adding fields is fine;
//! reordering or removing fields is a breaking change to the report
//! contract. Keep the field-name → value structure.
//!
//! # Future expansion
//!
//! Today's MVP covers the #860 sandbox-visibility need. Follow-up
//! fields (model+provider, project root, config path, data dir
//! writability) are tracked in #1258. Network-dependent checks
//! (provider health, MCP status) are deferred indefinitely — they'd
//! break the "instant + offline" property `doctor` shares with
//! `flutter doctor` / `brew doctor` / `npm doctor`.

use koda_core::sandbox;

/// Execute the `doctor` subcommand. Prints the diagnostic report to
/// stdout and exits successfully (exit code reflects the report's
/// own success: 0 always — we're reporting state, not asserting it).
pub fn run() {
    print!("{}", render_report());
}

/// Build the diagnostic report as a string. Separated from [`run`]
/// so unit tests can assert on the rendered output without capturing
/// stdout.
pub(crate) fn render_report() -> String {
    let report = sandbox::dependency_report();
    let sandbox_status = if report.available {
        format!("available ({})", report.backend)
    } else {
        let reason = report.reason.as_deref().unwrap_or("no reason reported");
        format!("UNAVAILABLE ({}: {})", report.backend, reason)
    };

    // `derive_default_trust` is the same helper #1241's flip will
    // consume — surfacing it here lets users preview what their
    // implicit default would be post-flip.
    let derived_default = koda_core::trust::derive_default_trust(report.available);

    let mut out = String::new();
    out.push_str("koda doctor \u{2014} platform diagnostics\n");
    out.push_str("==================================\n");
    out.push_str("Read-only inspection. Prints setup hints; doesn't run them.\n\n");
    out.push_str(&format!("koda version:    {}\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!(
        "OS:              {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!("Sandbox:         {sandbox_status}\n"));
    out.push_str(&format!(
        "Auto mode:       {}\n",
        if report.available {
            "supported"
        } else {
            "REFUSED — kernel sandbox required (see #860)"
        }
    ));
    out.push_str(&format!(
        "Implicit default trust: {}\n",
        derived_default.as_str()
    ));
    out.push('\n');

    if !report.available {
        out.push_str("Setup\n-----\n");
        out.push_str(&setup_hint(report.backend));
        out.push('\n');
    }

    out.push_str("Paste this whole block into bug reports.\n");
    out
}

/// Platform-specific install hint for the unavailable backend.
///
/// Kept tiny on purpose: the real install instructions live in the
/// docs (`docs/src/sandbox.md`) — this is the one-line nudge so a
/// user staring at a startup error knows what to type next.
fn setup_hint(backend: &str) -> String {
    match backend {
        "bwrap" => "  Install bubblewrap:\n    Debian/Ubuntu:  sudo apt install bubblewrap\n    Fedora/RHEL:    sudo dnf install bubblewrap\n    Arch:           sudo pacman -S bubblewrap\n  Or run with `--mode safe` to keep the human in the approval loop.\n".to_string(),
        "seatbelt" => "  Seatbelt is built into macOS. If it's reporting unavailable,\n  the `sandbox-exec` binary is missing from /usr/bin — file an issue.\n  Workaround: run with `--mode safe`.\n".to_string(),
        // `none` backend = unknown platform (Windows pre-sandbox-port,
        // exotic Unixes). No install path; only escape is `--mode safe`.
        _ => format!(
            "  No kernel sandbox backend exists for this platform ({}).\n  Run with `--mode safe` to keep the human in the approval loop.\n",
            std::env::consts::OS
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_contains_required_fields() {
        // Pin the field-name contract — bug reports rely on these
        // labels being grep-able. Reordering is fine; removing/
        // renaming is a breaking change to the support workflow.
        let out = render_report();
        assert!(out.contains("koda version:"), "missing version field");
        assert!(out.contains("OS:"), "missing OS field");
        assert!(out.contains("Sandbox:"), "missing sandbox field");
        assert!(out.contains("Auto mode:"), "missing auto-mode field");
        assert!(
            out.contains("Implicit default trust:"),
            "missing default-trust field"
        );
    }

    #[test]
    fn report_includes_paste_hint() {
        // Users skim; the paste-this nudge MUST be visible so the
        // doctor output ends up in bug reports.
        assert!(render_report().contains("Paste this"));
    }

    #[test]
    fn report_includes_read_only_subtitle() {
        // Sets expectations for users unfamiliar with the
        // flutter/brew/npm `doctor` convention. Without this line
        // it's reasonable to expect `koda doctor` to install
        // bubblewrap for you.
        assert!(
            render_report().contains("Read-only inspection"),
            "output must clarify it's diagnostic-only, not remediation"
        );
    }

    #[test]
    fn setup_hint_for_bwrap_mentions_apt_and_safe_fallback() {
        let hint = setup_hint("bwrap");
        assert!(hint.contains("apt install bubblewrap"));
        assert!(hint.contains("--mode safe"));
    }

    #[test]
    fn setup_hint_for_unknown_backend_offers_safe_fallback() {
        // The escape hatch must always be reachable, even on platforms
        // with no install path.
        let hint = setup_hint("imaginary-backend");
        assert!(hint.contains("--mode safe"));
    }

    #[test]
    fn report_pins_default_trust_to_sandbox_availability() {
        // The "Implicit default trust" line MUST reflect
        // `derive_default_trust(sandbox_available)` so users can
        // preview what #1241's flip will give them on their machine.
        let out = render_report();
        let report = sandbox::dependency_report();
        let expected = if report.available { "auto" } else { "safe" };
        assert!(
            out.contains(&format!("Implicit default trust: {expected}")),
            "default-trust line must match derive_default_trust({}); got:\n{}",
            report.available,
            out
        );
    }

    #[test]
    fn auto_mode_line_reflects_sandbox_state() {
        // Pin the user-facing summary so a future refactor can't
        // make Auto look "supported" on a system where startup
        // would refuse it.
        let out = render_report();
        let report = sandbox::dependency_report();
        if report.available {
            assert!(out.contains("Auto mode:       supported"));
        } else {
            assert!(out.contains("REFUSED"));
            assert!(out.contains("#860"));
        }
    }

    // Sanity: the doctor module exists for support diagnostics; see
    // also koda-core::trust::derive_default_trust which we delegate
    // to for the "Implicit default trust" line.
}
