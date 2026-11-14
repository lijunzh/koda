//! The model choice earlier versions saved in the database (#693).
//!
//! Koda keeps the choice a new session starts from in
//! `~/.config/koda/config.toml` now (`model`, `model_provider`,
//! `[model_providers.<id>] base_url`), where a person can read and edit it.
//! Earlier versions saved it in the database's `kv_store` as
//! `setting:last_provider`; the host reads that row once, writes it into the
//! file, and deletes it ([`load_last_provider`](crate::last_provider::load_last_provider), [`delete_last_provider`](crate::last_provider::delete_last_provider)).
//!
//! When that choice was an alias (`claude-haiku`), the alias was saved next
//! to the id it resolved to; the move keeps the alias, so it follows its
//! line.

use crate::db::Database;
use anyhow::Result;

/// KV key for the last-used provider.
const KV_KEY: &str = "setting:last_provider";

/// The model choice as earlier versions saved it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LastProvider {
    /// Provider type name (e.g. `"Anthropic"`, `"Gemini"`).
    pub provider_type: String,
    /// API base URL.
    pub base_url: String,
    /// Model identifier.
    pub model: String,
    /// The `/model` alias `model` was resolved from, if the choice was an
    /// alias. Rows saved before this field existed have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// The choice earlier versions saved, if any.
pub async fn load_last_provider(db: &Database) -> Result<Option<LastProvider>> {
    match db.kv_get(KV_KEY).await? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(None),
    }
}

/// Delete the saved choice, once it is in the config file.
pub async fn delete_last_provider(db: &Database) -> Result<()> {
    db.kv_delete(KV_KEY).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_last_provider_serde_round_trip() {
        let lp = LastProvider {
            provider_type: "Anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-5".into(),
            alias: None,
        };
        let json = serde_json::to_string(&lp).unwrap();
        let parsed: LastProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.provider_type, "Anthropic");
        assert_eq!(parsed.base_url, "https://api.anthropic.com");
        assert_eq!(parsed.model, "claude-opus-5");
    }

    #[test]
    fn test_last_provider_json_contains_expected_keys() {
        let lp = LastProvider {
            provider_type: "Gemini".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            model: "gemini-2.5-pro".into(),
            alias: None,
        };
        let json = serde_json::to_string(&lp).unwrap();
        assert!(json.contains("provider_type"));
        assert!(json.contains("base_url"));
        assert!(json.contains("model"));
        assert!(json.contains("Gemini"));
    }

    #[test]
    fn test_last_provider_alias_round_trips_and_old_rows_have_none() {
        let lp = LastProvider {
            provider_type: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-haiku-5-5".into(),
            alias: Some("claude-haiku".into()),
        };
        let parsed: LastProvider =
            serde_json::from_str(&serde_json::to_string(&lp).unwrap()).unwrap();
        assert_eq!(parsed.alias.as_deref(), Some("claude-haiku"));

        // A row written before the field existed.
        let old: LastProvider = serde_json::from_str(
            r#"{"provider_type":"anthropic","base_url":"https://api.anthropic.com","model":"claude-haiku-5-5-20261001"}"#,
        )
        .unwrap();
        assert_eq!(old.alias, None);
    }

    #[test]
    fn test_last_provider_deserialize_invalid_json_returns_error() {
        let result: Result<LastProvider, _> = serde_json::from_str("{not valid json}");
        assert!(result.is_err());
    }
}
