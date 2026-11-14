//! Setting variables Koda no longer reads (CF-3b, 2026-11). Codex keeps no
//! settings in the environment: they live in `config.toml`, with `-c` for
//! one run, and Koda follows. A variable still set gets a warning at
//! startup naming what replaced it, so a changed behavior isn't silent.
//! Remove this list two releases after 0.7.

/// A retired variable and what to use instead.
const RETIRED: &[(&str, &str)] = &[
    ("KODA_MODEL", "--model, or `model` in config.toml"),
    (
        "KODA_PROVIDER",
        "--provider, or `model_provider` in config.toml",
    ),
    (
        "KODA_BASE_URL",
        "--base-url, or `[model_providers.<id>] base_url` in config.toml",
    ),
    (
        "KODA_MAX_CONTEXT_TOKENS",
        "--max-context-tokens, or `model_context_window` in config.toml",
    ),
    (
        "KODA_REASONING_EFFORT",
        "--effort, or `model_reasoning_effort` in config.toml",
    ),
    ("KODA_MODE", "--mode, or `mode` in config.toml"),
    (
        "KODA_POST_TURN_COMPACT_PERCENT",
        "`model_post_turn_compact_threshold_percent` in config.toml",
    ),
    (
        "KODA_REQUEST_MAX_RETRIES",
        "`[model_providers.<id>] request_max_retries` in config.toml",
    ),
    (
        "KODA_STREAM_MAX_RETRIES",
        "`[model_providers.<id>] stream_max_retries` in config.toml",
    ),
    (
        "KODA_READ_TIMEOUT_SECS",
        "`[model_providers.<id>] stream_idle_timeout_ms` in config.toml",
    ),
    (
        "KODA_LOCAL_URL",
        "`[model_providers.<id>] base_url` in config.toml",
    ),
    (
        "KODA_SYNTAX_HIGHLIGHT",
        "`[tui] syntax_highlight` in config.toml",
    ),
    (
        "KODA_ALLOW_LOCAL_BINDING",
        "`[sandbox] allow_local_binding` in config.toml",
    ),
    (
        "KODA_CONNECT_TIMEOUT_SECS",
        "nothing: the connect timeout is a fixed 30 seconds",
    ),
];

/// One warning per retired variable set in `vars` (name, value).
fn warnings_for<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let set: Vec<&str> = vars.into_iter().map(|(name, _)| name).collect();
    RETIRED
        .iter()
        .filter(|(name, _)| set.contains(name))
        .map(|(name, instead)| {
            format!("{name} is no longer read; use {instead} (or -c for one run).")
        })
        .collect()
}

/// Warnings for the retired variables set in this process's environment.
pub(crate) fn warnings() -> Vec<String> {
    let vars: Vec<(String, String)> = std::env::vars().collect();
    warnings_for(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_retired_variable_set_is_named_with_its_replacement() {
        let got = warnings_for([
            ("KODA_MODEL", "x"),
            ("KODA_LOG", "debug"),
            ("KODA_ALLOW_LOCAL_BINDING", "1"),
        ]);
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(
            got[0].starts_with("KODA_MODEL is no longer read; use --model"),
            "{got:?}"
        );
        assert!(got[1].contains("[sandbox] allow_local_binding"), "{got:?}");
        assert!(warnings_for([("KODA_LOG", "debug")]).is_empty());
    }
}
