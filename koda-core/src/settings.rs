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
