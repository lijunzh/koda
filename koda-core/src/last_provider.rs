//! Last-used provider recall (#693).
//!
//! Remembers the last provider/model/base-URL so Koda can auto-restore
//! on next startup. Stored in SQLite via the [`crate::db`] KV store.
//!
//! This is **not** user configuration — Koda follows "customization over
//! configuration" (see DESIGN.md §P1). The only persisted state is which
//! provider the user last chose via `/model`.
//!
//! When that choice was an alias (`claude-haiku`), the alias is saved next
//! to the id it resolved to, so startup can resolve it again and pick up a
//! newer release; the id is what runs when it can't.
//!
//! Renamed from `settings.rs` to avoid implying a user-editable config
//! surface — koda has none.

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
    /// The `/model` alias `model` was resolved from, if the choice was an
    /// alias. Rows saved before this field existed have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// Load the last-used provider from the DB. Returns `None` if not set.
pub async fn load_last_provider(db: &Database) -> Result<Option<LastProvider>> {
    match db.kv_get(KV_KEY).await? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(None),
    }
}

/// Save the last-used provider to the DB. `alias` is the alias the user
/// picked, or `None` for a literal model id.
pub async fn save_last_provider(
    db: &Database,
    provider_type: &str,
    base_url: &str,
    model: &str,
    alias: Option<&str>,
) -> Result<()> {
    let lp = LastProvider {
        provider_type: provider_type.to_string(),
        base_url: base_url.to_string(),
        model: model.to_string(),
        alias: alias.map(str::to_string),
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
            alias: None,
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
            r#"{"provider_type":"anthropic","base_url":"https://api.anthropic.com","model":"claude-haiku-4-5-20251001"}"#,
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
