//! `koda exec --output-schema`: the schema file, and whether the last reply
//! matches it. The host enforces the schema during the run
//! (`koda_core::output_schema`); this checks what reaches stdout, so a
//! script can trust the exit code.

use anyhow::{Context, Result};
use std::path::Path;

/// The JSON Schema in `path`, refused early if Koda can't check against it.
pub(crate) fn load(path: &Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("couldn't read the output schema {}", path.display()))?;
    let schema: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("the output schema {} isn't JSON", path.display()))?;
    if let Err(e) = jsonschema::validator_for(&schema) {
        anyhow::bail!(
            "the output schema {} isn't a JSON Schema Koda can check: {e}",
            path.display()
        );
    }
    Ok(schema)
}

/// Why `reply` doesn't match `schema`, or `None` when it does: the reply
/// must be the JSON value alone.
pub(crate) fn mismatch(schema: &serde_json::Value, reply: &str) -> Option<String> {
    let value: serde_json::Value = match serde_json::from_str(reply.trim()) {
        Ok(value) => value,
        Err(e) => return Some(format!("it isn't JSON ({e})")),
    };
    let validator = jsonschema::validator_for(schema).ok()?;
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .take(5)
        .map(|e| e.to_string())
        .collect();
    (!errors.is_empty()).then(|| errors.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_reply_matches_or_says_why_not() {
        let schema = json!({"type": "object", "required": ["ok"]});
        assert_eq!(mismatch(&schema, "{\"ok\": 1}"), None);
        assert!(mismatch(&schema, "{}").is_some());
        assert!(mismatch(&schema, "ok").unwrap().contains("isn't JSON"));
    }

    #[test]
    fn a_bad_schema_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema.json");
        std::fs::write(&path, r#"{"type": "nope"}"#).unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, "not json").unwrap();
        assert!(load(&path).unwrap_err().to_string().contains("isn't JSON"));
        std::fs::write(&path, r#"{"type": "object"}"#).unwrap();
        assert!(load(&path).is_ok());
    }
}
