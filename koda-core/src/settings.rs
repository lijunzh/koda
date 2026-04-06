//! Last-used provider persistence (#693).
//!
//! Remembers the last provider/model/base-URL so Koda can auto-restore
//! on next startup. Stored in SQLite via the [`crate::db`] KV store.
//!
//! This is **not** user configuration — Koda follows "customization over
//! configuration" (see DESIGN.md). The only persisted state is which
//! provider the user last chose via `/model`.

use crate::db::Database;
use anyhow::Result;

/// KV key for the last-used provider.
const KV_KEY: &str = "setting:last_provider";

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

/// Load the last-used provider from the DB. Returns `None` if not set.
pub async fn load_last_provider(db: &Database) -> Result<Option<LastProvider>> {
    match db.kv_get(KV_KEY).await? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(None),
    }
}

/// Save the last-used provider to the DB.
pub async fn save_last_provider(
    db: &Database,
    provider_type: &str,
    base_url: &str,
    model: &str,
) -> Result<()> {
    let lp = LastProvider {
        provider_type: provider_type.to_string(),
        base_url: base_url.to_string(),
        model: model.to_string(),
    };
    let json = serde_json::to_string(&lp)?;
    db.kv_set(KV_KEY, &json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_last_provider_serde_round_trip() {
        let lp = LastProvider {
            provider_type: "Anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-4-5".into(),
        };
        let json = serde_json::to_string(&lp).unwrap();
        let parsed: LastProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.provider_type, "Anthropic");
        assert_eq!(parsed.base_url, "https://api.anthropic.com");
        assert_eq!(parsed.model, "claude-opus-4-5");
    }

    #[test]
    fn test_last_provider_json_contains_expected_keys() {
        let lp = LastProvider {
            provider_type: "Gemini".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            model: "gemini-2.5-pro".into(),
        };
        let json = serde_json::to_string(&lp).unwrap();
        assert!(json.contains("provider_type"));
        assert!(json.contains("base_url"));
        assert!(json.contains("model"));
        assert!(json.contains("Gemini"));
    }

    #[test]
    fn test_last_provider_deserialize_invalid_json_returns_error() {
        let result: Result<LastProvider, _> = serde_json::from_str("{not valid json}");
        assert!(result.is_err());
    }
}
