//! Last-used provider persistence.
//!
//! Remembers the last provider/model/base-URL so Koda can auto-restore
//! on next startup. Currently stored in `~/.config/koda/settings.toml`.
//!
//! **Note:** This module is slated for removal (#693). The TOML file
//! should be a row in SQLite, not a separate config file — users should
//! never edit it manually.
//!
//! This is **not** user configuration — Koda follows "customization over
//! configuration" (see DESIGN.md). The only persisted state is which
//! provider the user last chose via `/model`.

use std::path::{Path, PathBuf};

/// Last-used provider state, restored on startup.
///
/// Not user configuration — just session memory.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Last-used provider/model, restored on next startup.
    #[serde(default)]
    pub last_provider: Option<LastProvider>,
}

/// Last-used provider configuration, restored on startup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastProvider {
    /// Provider type name (e.g. `"Anthropic"`, `"Gemini"`).
    pub provider_type: String,
    /// API base URL.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
}

impl Settings {
    /// Load from `~/.config/koda/settings.toml`, returning defaults if missing.
    pub fn load() -> Self {
        Self::settings_path()
            .and_then(|path| std::fs::read_to_string(&path).ok())
            .and_then(|content| toml::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Save to `~/.config/koda/settings.toml`.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::settings_path()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine config directory"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Save the last-used provider/model for restoration on next startup.
    pub fn save_last_provider(
        &mut self,
        provider_type: &str,
        base_url: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        self.last_provider = Some(LastProvider {
            provider_type: provider_type.to_string(),
            base_url: base_url.to_string(),
            model: model.to_string(),
        });
        self.save()
    }

    fn settings_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        Some(
            Path::new(&home)
                .join(".config")
                .join("koda")
                .join("settings.toml"),
        )
    }
}
