//! Secure API key storage.
//!
//! Keys are stored in `~/.config/koda/keys.toml` with
//! restrictive file permissions (0600). This file is user-level,
//! never inside a project directory, and never committed to git.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

const CONFIG_DIR: &str = "koda";
const KEYS_FILE: &str = "keys.toml";

/// Stored API keys, keyed by env var name (e.g. "ANTHROPIC_API_KEY").
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct KeyStore {
    #[serde(default)]
    /// Map of environment variable name → API key value.
    pub keys: HashMap<String, String>,
}

impl KeyStore {
    /// Load keys from disk. Returns empty store if file doesn't exist.
    pub fn load() -> Result<Self> {
        let path = Self::keys_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let store: Self = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(store)
    }

    /// Save keys to disk with restrictive permissions.
    ///
    /// On Unix, the file is created with mode 0600 atomically via `OpenOptions`
    /// to avoid a TOCTOU window where the file is world-readable between
    /// `write()` and `set_permissions()`.
    pub fn save(&self) -> Result<()> {
        let path = Self::keys_path()?;

        // Ensure config directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("Failed to create {}", path.display()))?;
            file.write_all(content.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(&path, &content)?;
        }

        tracing::info!("Saved keys to {}", path.display());
        Ok(())
    }

    /// Set a key by env var name.
    pub fn set(&mut self, env_name: &str, value: &str) {
        self.keys.insert(env_name.to_string(), value.to_string());
    }

    /// Load all stored keys into the process environment.
    /// Load all stored keys into the runtime environment.
    /// Only sets vars that aren't already set (env vars and
    /// previously-set runtime vars take precedence).
    pub fn inject_into_env(&self) {
        for (name, value) in &self.keys {
            if crate::runtime_env::get(name).is_none() {
                crate::runtime_env::set(name, value);
                tracing::debug!("Injected stored key: {name}");
            }
        }
    }

    /// Path to the keys file: ~/.config/koda/keys.toml
    pub fn keys_path() -> Result<PathBuf> {
        let config_dir = dirs_config_dir().context("Could not determine config directory")?;
        Ok(config_dir.join(CONFIG_DIR).join(KEYS_FILE))
    }
}

/// Cross-platform config directory.
fn dirs_config_dir() -> Option<PathBuf> {
    // $XDG_CONFIG_HOME or ~/.config on unix, AppData on windows
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .or_else(|| std::env::var("APPDATA").ok().map(PathBuf::from))
}

/// Mask a key for display: "sk-ant-abc...xyz"
pub fn mask_key(key: &str) -> String {
    if key.len() > 8 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_keystore_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("keys.toml");

        let mut store = KeyStore::default();
        store.set("OPENAI_API_KEY", "sk-test-123");
        store.set("ANTHROPIC_API_KEY", "sk-ant-456");

        // Save
        let content = toml::to_string_pretty(&store).unwrap();
        std::fs::write(&path, &content).unwrap();

        // Load
        let loaded: KeyStore = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            loaded.keys.get("OPENAI_API_KEY").map(|s| s.as_str()),
            Some("sk-test-123")
        );
        assert_eq!(
            loaded.keys.get("ANTHROPIC_API_KEY").map(|s| s.as_str()),
            Some("sk-ant-456")
        );
    }

    #[test]
    fn test_mask_key() {
        assert_eq!(mask_key("sk-ant-api03-longkey1234"), "sk-a...1234");
        assert_eq!(mask_key("short"), "****");
    }

    #[test]
    fn test_remove_key() {
        let mut store = KeyStore::default();
        store.set("TEST_KEY", "value");
        assert!(store.keys.remove("TEST_KEY").is_some());
        assert!(store.keys.remove("TEST_KEY").is_none());
        assert_eq!(store.keys.get("TEST_KEY"), None);
    }
}
