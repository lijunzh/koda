//! User settings persistence.
//!
//! Stores and loads user preferences from `~/.config/koda/settings.toml`.

use std::path::{Path, PathBuf};

/// User settings stored in `~/.config/koda/settings.toml`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Last-used provider/model, restored on next startup.
    #[serde(default)]
    pub last_provider: Option<LastProvider>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastProvider {
    pub provider_type: String,
    pub base_url: String,
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
