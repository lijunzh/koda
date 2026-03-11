//! Model capability probe.
//!
//! One-time binary gate at session start: can this model produce
//! structured output and follow instructions? If not, fail loudly
//! instead of degrading silently.
//!
//! Results are cached per model name in `~/.config/koda/model_probes.json`.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;

/// The probe prompt — tests structured JSON output + instruction following.
const PROBE_PROMPT: &str = r#"You are being tested. Respond with ONLY this JSON, no other text:
{"action": "read_file", "target": "src/main.rs", "reasoning": "need to explore first"}"#;

/// Expected keys in the probe response.
const EXPECTED_KEYS: &[&str] = &["action", "target", "reasoning"];

/// Run the capability probe for a model.
///
/// Returns `Ok(())` if the model passes, `Err` with a human-readable
/// message if it fails. Checks the cache first.
pub async fn ensure_capable(
    model: &str,
    provider: &dyn crate::providers::LlmProvider,
    model_settings: &crate::config::ModelSettings,
    sink: &dyn crate::engine::EngineSink,
) -> Result<()> {
    // Check cache first
    if is_cached_pass(model) {
        return Ok(());
    }

    sink.emit(crate::engine::EngineEvent::Info {
        message: format!("Validating model capability for {model}..."),
    });

    // Run the probe
    let messages = vec![crate::providers::ChatMessage::text("user", PROBE_PROMPT)];

    let response = provider.chat(&messages, &[], model_settings).await?;

    let content = response.content.as_deref().unwrap_or("").trim();

    // Try to parse as JSON
    match validate_probe_response(content) {
        Ok(()) => {
            cache_result(model, true);
            Ok(())
        }
        Err(reason) => {
            cache_result(model, false);
            anyhow::bail!(
                "Model '{model}' failed capability probe: {reason}\n\
                 Koda requires a model that can produce structured JSON output.\n\
                 Try a more capable model, or use --skip-probe to bypass this check."
            );
        }
    }
}

/// Validate the probe response.
fn validate_probe_response(content: &str) -> Result<(), String> {
    // Strip markdown code fences if present
    let json_str = content
        .strip_prefix("```json")
        .or_else(|| content.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .unwrap_or(content)
        .trim();

    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("response is not valid JSON: {e}"))?;

    let obj = parsed.as_object().ok_or("response is not a JSON object")?;

    for key in EXPECTED_KEYS {
        if !obj.contains_key(*key) {
            return Err(format!("missing required key: '{key}'"));
        }
    }

    Ok(())
}

// ── Cache ─────────────────────────────────────────────────────

fn cache_path() -> PathBuf {
    crate::db::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("model_probes.json")
}

fn load_cache() -> HashMap<String, bool> {
    let path = cache_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn is_cached_pass(model: &str) -> bool {
    load_cache().get(model).copied() == Some(true)
}

fn cache_result(model: &str, passed: bool) {
    let mut cache = load_cache();
    cache.insert(model.to_string(), passed);
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&cache) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_response() {
        let response =
            r#"{"action": "read_file", "target": "src/main.rs", "reasoning": "exploring"}"#;
        assert!(validate_probe_response(response).is_ok());
    }

    #[test]
    fn test_valid_response_with_code_fence() {
        let response = "```json\n{\"action\": \"read_file\", \"target\": \"src/main.rs\", \"reasoning\": \"exploring\"}\n```";
        assert!(validate_probe_response(response).is_ok());
    }

    #[test]
    fn test_invalid_not_json() {
        let response = "Sure! Here is the JSON: {action: read_file}";
        assert!(validate_probe_response(response).is_err());
    }

    #[test]
    fn test_invalid_missing_key() {
        let response = r#"{"action": "read_file", "target": "src/main.rs"}"#;
        let err = validate_probe_response(response).unwrap_err();
        assert!(err.contains("reasoning"));
    }

    #[test]
    fn test_invalid_not_object() {
        let response = r#"["action", "read_file"]"#;
        assert!(validate_probe_response(response).is_err());
    }

    #[test]
    fn test_empty_response() {
        assert!(validate_probe_response("").is_err());
    }
}
