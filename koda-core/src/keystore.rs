//! Secure API key storage (#693).
//!
//! Keys are stored in SQLite via the [`crate::db`] KV store with the
//! `apikey:` prefix. The database file is created with 0600 permissions.
//!
//! ## Supported providers
//!
//! Each provider has its own key entry. Use `/key` in the REPL
//! or the onboarding wizard to manage keys.
//!
//! Keys are also read from environment variables (e.g., `ANTHROPIC_API_KEY`).
//! The keystore is checked first, then the environment.

use crate::db::Database;
use anyhow::Result;

/// KV key prefix for API keys.
const PREFIX: &str = "apikey:";

/// Set an API key in the DB.
pub async fn set_key(db: &Database, env_name: &str, value: &str) -> Result<()> {
    let key = format!("{PREFIX}{env_name}");
    db.kv_set(&key, value).await
}

/// Get an API key from the DB.
pub async fn get_key(db: &Database, env_name: &str) -> Result<Option<String>> {
    let key = format!("{PREFIX}{env_name}");
    db.kv_get(&key).await
}

/// Remove an API key from the DB.
pub async fn remove_key(db: &Database, env_name: &str) -> Result<()> {
    let key = format!("{PREFIX}{env_name}");
    db.kv_delete(&key).await
}

/// Load all stored keys and inject into the runtime environment.
///
/// Only sets vars that aren't already set (env vars and
/// previously-set runtime vars take precedence).
pub async fn inject_into_env(db: &Database) -> Result<()> {
    let entries = db.kv_list_prefix(PREFIX).await?;
    for (key, value) in entries {
        let env_name = &key[PREFIX.len()..];
        if crate::runtime_env::get(env_name).is_none() {
            crate::runtime_env::set(env_name, &value);
            tracing::debug!("Injected stored key: {env_name}");
        }
    }
    Ok(())
}

/// Mask a key for display: shows first 4 and last 4 characters.
///
/// # Examples
///
/// ```
/// use koda_core::keystore::mask_key;
///
/// assert_eq!(mask_key("sk-ant-api03-longkey1234"), "sk-a...1234");
/// assert_eq!(mask_key("short"), "****");
/// ```
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

    #[test]
    fn test_mask_key() {
        assert_eq!(mask_key("sk-ant-api03-longkey1234"), "sk-a...1234");
        assert_eq!(mask_key("short"), "****");
    }

    #[tokio::test]
    async fn test_keystore_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Database::init(tmp.path()).await.unwrap();

        set_key(&db, "TEST_KEY", "test-value").await.unwrap();
        let val = get_key(&db, "TEST_KEY").await.unwrap();
        assert_eq!(val.as_deref(), Some("test-value"));

        remove_key(&db, "TEST_KEY").await.unwrap();
        let val = get_key(&db, "TEST_KEY").await.unwrap();
        assert_eq!(val, None);
    }

    #[tokio::test]
    async fn test_inject_skips_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = Database::init(tmp.path()).await.unwrap();

        // Set a key that already exists in runtime env
        let unique = format!("KODA_TEST_{}", std::process::id());
        crate::runtime_env::set(&unique, "original");
        set_key(&db, &unique, "overwritten").await.unwrap();

        inject_into_env(&db).await.unwrap();

        // Should keep the original value
        assert_eq!(
            crate::runtime_env::get(&unique).as_deref(),
            Some("original")
        );
    }
}
