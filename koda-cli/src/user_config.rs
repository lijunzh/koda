//! This run's config file: `~/.config/koda/config.toml` with the `-c
//! key=value` values given on the command line over it, as Codex's
//! `CliConfigOverrides` sit over its `config.toml`. The host that `koda`
//! starts gets this file; the TUI also reads its own `[tui]` keys from it,
//! as Codex's TUI reads its `[tui]` table from its own machine's file.

use std::sync::OnceLock;

use koda_config::{ConfigFile, Override};

static OVERRIDES: OnceLock<Vec<Override>> = OnceLock::new();

/// Keep this run's `-c` values. Called once, at startup.
pub(crate) fn set_overrides(overrides: Vec<Override>) {
    let _ = OVERRIDES.set(overrides);
}

/// The config file with this run's `-c` values over it.
pub(crate) fn file() -> anyhow::Result<ConfigFile> {
    Ok(ConfigFile::in_dir(&koda_core::db::config_dir()?)
        .with_overrides(OVERRIDES.get().cloned().unwrap_or_default()))
}

/// The TUI's own settings, from the file and `-c`. Defaults when the file
/// can't be read: the host reports that, at startup.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn tui() -> koda_config::TuiToml {
    file()
        .ok()
        .and_then(|file| file.load().ok())
        .map(|loaded| loaded.config.tui)
        .unwrap_or_default()
}
