//! First-run detection.
//!
//! The onboarding wizard is now part of the TUI (`tui_app.rs`) —
//! on first run, the provider dropdown auto-opens. This module
//! only provides the detection logic.

/// Check if this is the first run (no config directory exists).
pub fn is_first_run() -> bool {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if home.is_empty() {
        return false;
    }
    let config_dir = std::path::PathBuf::from(&home).join(".config").join("koda");
    !config_dir.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_first_run_logic() {
        let _ = is_first_run();
    }
}
