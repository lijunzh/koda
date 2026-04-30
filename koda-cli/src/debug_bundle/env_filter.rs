//! Environment variable filter for debug bundles.
//!
//! Applies a hardcoded **allowlist** to `std::env::vars()`. Per RFC #1167
//! decision D5, there is intentionally **no runtime opt-in** to expose more
//! variables — to add a new variable, edit the source allowlist and rebuild.
//!
//! Three categories:
//!
//! - **Allowlisted verbatim** ([`ALLOWLISTED_PREFIXES`] / [`ALLOWLISTED_NAMES`]):
//!   value is captured as-is. These are koda-internal config (`KODA_*`),
//!   Rust runtime (`RUST_*`), or terminal/locale knobs whose values are
//!   never sensitive.
//!
//! - **Tracked but value-redacted** ([`REDACT_NAMES`]): we record presence
//!   and length but not the value. "Is `OPENAI_API_KEY` set?" is often the
//!   debug question; the actual value is a credential leak waiting to happen.
//!
//! - **Everything else**: omitted entirely. This is the safe-by-default
//!   posture — a future env var with a sensitive name we didn't anticipate
//!   leaks under denylist but stays redacted under allowlist.
//!
//! ## Rationale (from RFC #1167 §D5)
//!
//! > Allowlist > denylist for safety: a future env var with a sensitive name
//! > we didn't anticipate leaks by default under denylist, stays redacted
//! > under allowlist.
//!
//! > No `KODA_DEBUG_BUNDLE_INCLUDE_ENV` runtime opt-in. We own the solution
//! > and can change source when needed. Minimal runtime config = less
//! > surface area, less to document, less to test, no foot-guns from users
//! > opting into leaking their own credentials.

use std::collections::BTreeMap;

/// Names whose **value** is captured verbatim. These are non-sensitive
/// runtime/terminal/locale knobs.
pub(super) const ALLOWLISTED_NAMES: &[&str] = &[
    "TERM",
    "COLORTERM",
    "LANG",
    "SHELL",
    "EDITOR",
    "PAGER",
    "VISUAL",
    "TZ",
    "TMPDIR",
];

/// Prefixes whose **value** is captured verbatim. Anything starting with
/// these prefixes is included in full.
pub(super) const ALLOWLISTED_PREFIXES: &[&str] = &[
    "KODA_", "RUST_", "LC_", // locale categories: LC_ALL, LC_CTYPE, LC_NUMERIC, etc.
];

/// Names whose **presence and length** are recorded but value is replaced
/// with `<redacted: N bytes>`. Knowing whether these are set is often the
/// debug question; the actual value is a credential.
pub(super) const REDACT_NAMES: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "ELEMENT_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "https_proxy",
    "http_proxy",
];

/// Filter an iterator of `(name, value)` pairs through the allowlist.
///
/// Returns a `BTreeMap` (sorted output is reproducible across runs, which
/// makes diffing two bundles meaningful). Names that match neither the
/// allowlist nor the redact list are dropped entirely.
///
/// # Examples
///
/// See the unit tests at the bottom of this file for canonical inputs.
pub(super) fn filter<I, K, V>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = BTreeMap::new();
    for (name, value) in vars {
        let name_ref = name.as_ref();
        let value_ref = value.as_ref();
        if let Some(rendered) = classify(name_ref, value_ref) {
            out.insert(name_ref.to_string(), rendered);
        }
    }
    out
}

/// Decide what to record for a single env var, or `None` to drop it.
///
/// Separated from [`filter`] so it's directly testable without building
/// an iterator.
fn classify(name: &str, value: &str) -> Option<String> {
    if REDACT_NAMES.contains(&name) {
        return Some(format!("<redacted: {} bytes>", value.len()));
    }
    if ALLOWLISTED_NAMES.contains(&name) {
        return Some(value.to_string());
    }
    if ALLOWLISTED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Some(value.to_string());
    }
    None
}

/// Render the filtered map to a deterministic `KEY=VALUE\n` text format
/// suitable for direct inclusion in `env.txt`. Lines are sorted by key
/// (BTreeMap iteration order) so two bundles diff cleanly.
pub(super) fn render(filtered: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in filtered {
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn koda_prefix_passes_through_verbatim() {
        let result = filter([("KODA_RENDER", "inline"), ("KODA_LOG", "debug")]);
        assert_eq!(result.get("KODA_RENDER"), Some(&"inline".to_string()));
        assert_eq!(result.get("KODA_LOG"), Some(&"debug".to_string()));
    }

    #[test]
    fn rust_prefix_passes_through_verbatim() {
        let result = filter([("RUST_BACKTRACE", "1"), ("RUST_LOG", "trace")]);
        assert_eq!(result.get("RUST_BACKTRACE"), Some(&"1".to_string()));
        assert_eq!(result.get("RUST_LOG"), Some(&"trace".to_string()));
    }

    #[test]
    fn lc_prefix_passes_through_verbatim() {
        let result = filter([("LC_ALL", "en_US.UTF-8"), ("LC_CTYPE", "C")]);
        assert_eq!(result.get("LC_ALL"), Some(&"en_US.UTF-8".to_string()));
        assert_eq!(result.get("LC_CTYPE"), Some(&"C".to_string()));
    }

    #[test]
    fn explicit_allowlist_names_pass_through() {
        let result = filter([
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8"),
            ("EDITOR", "vim"),
            ("SHELL", "/bin/zsh"),
        ]);
        assert_eq!(result.get("TERM"), Some(&"xterm-256color".to_string()));
        assert_eq!(result.get("LANG"), Some(&"en_US.UTF-8".to_string()));
        assert_eq!(result.get("EDITOR"), Some(&"vim".to_string()));
        assert_eq!(result.get("SHELL"), Some(&"/bin/zsh".to_string()));
    }

    #[test]
    fn redact_names_keep_length_drop_value() {
        let result = filter([
            ("OPENAI_API_KEY", "sk-proj-abcdef1234567"), // 21 bytes (counted explicitly)
            ("ANTHROPIC_API_KEY", "sk-ant-api03-xyz"),   // 16 bytes
            ("GITHUB_TOKEN", "ghp_aaa"),                 //  7 bytes
        ]);
        assert_eq!(
            result.get("OPENAI_API_KEY"),
            Some(&"<redacted: 21 bytes>".to_string()),
            "value bytes leaked: check format string"
        );
        assert_eq!(
            result.get("ANTHROPIC_API_KEY"),
            Some(&"<redacted: 16 bytes>".to_string())
        );
        assert_eq!(
            result.get("GITHUB_TOKEN"),
            Some(&"<redacted: 7 bytes>".to_string())
        );
    }

    #[test]
    fn proxy_vars_redacted_both_cases() {
        // Some envs use lowercase http_proxy; we cover both because real
        // user setups vary (curl reads lowercase, most tools read uppercase).
        let result = filter([
            ("HTTPS_PROXY", "http://user:pass@proxy.example.com:8080"),
            ("http_proxy", "http://internal.example.com:3128"),
        ]);
        assert!(result.get("HTTPS_PROXY").unwrap().starts_with("<redacted:"));
        assert!(result.get("http_proxy").unwrap().starts_with("<redacted:"));
    }

    #[test]
    fn pii_corp_id_dropped_entirely() {
        // HOME, USER, PWD all contain corp ID at Walmart and shouldn't
        // appear in a bundle that might be shared cross-team or with
        // an external LLM for debugging.
        let result = filter([
            ("HOME", "/Users/l0z05rg"),
            ("USER", "l0z05rg"),
            ("PWD", "/Users/l0z05rg/repo/koda"),
            ("LOGNAME", "l0z05rg"),
        ]);
        assert!(!result.contains_key("HOME"), "HOME leaked corp ID");
        assert!(!result.contains_key("USER"), "USER leaked corp ID");
        assert!(!result.contains_key("PWD"), "PWD leaked corp ID");
        assert!(!result.contains_key("LOGNAME"), "LOGNAME leaked corp ID");
    }

    #[test]
    fn unknown_var_with_credential_in_value_dropped_by_default() {
        // The whole point of allowlist > denylist: a future variable name
        // we never anticipated stays out of the bundle even when its
        // value looks credential-like.
        let result = filter([
            ("FUTURE_API_KEY", "sk-something-secret"),
            ("VENDOR_X_TOKEN", "xyz-classified"),
            ("WHATEVER", "value"),
        ]);
        assert!(!result.contains_key("FUTURE_API_KEY"));
        assert!(!result.contains_key("VENDOR_X_TOKEN"));
        assert!(!result.contains_key("WHATEVER"));
    }

    #[test]
    fn empty_input_yields_empty_output() {
        let empty: Vec<(&str, &str)> = vec![];
        let result = filter(empty);
        assert!(result.is_empty());
    }

    #[test]
    fn render_produces_sorted_key_eq_value_lines() {
        // BTreeMap iteration is sorted, which renders as sorted lines.
        // This makes diffing two bundles meaningful.
        let mut input = BTreeMap::new();
        input.insert("ZULU".to_string(), "z".to_string());
        input.insert("ALPHA".to_string(), "a".to_string());
        input.insert("MIKE".to_string(), "m".to_string());
        let rendered = render(&input);
        assert_eq!(rendered, "ALPHA=a\nMIKE=m\nZULU=z\n");
    }

    #[test]
    fn render_preserves_redaction_marker() {
        let mut input = BTreeMap::new();
        input.insert(
            "OPENAI_API_KEY".to_string(),
            "<redacted: 21 bytes>".to_string(),
        );
        let rendered = render(&input);
        assert_eq!(rendered, "OPENAI_API_KEY=<redacted: 21 bytes>\n");
    }

    #[test]
    fn classify_directly_for_all_three_paths() {
        // Allowlist-prefix path
        assert_eq!(
            classify("KODA_RENDER", "inline"),
            Some("inline".to_string())
        );
        // Allowlist-name path
        assert_eq!(classify("TERM", "xterm"), Some("xterm".to_string()));
        // Redact path
        assert_eq!(
            classify("OPENAI_API_KEY", "sk-12345"),
            Some("<redacted: 8 bytes>".to_string())
        );
        // Drop path
        assert_eq!(classify("HOME", "/Users/foo"), None);
        assert_eq!(classify("RANDOM_FUTURE_VAR", "value"), None);
    }
}
