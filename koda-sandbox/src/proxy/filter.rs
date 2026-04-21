//! Hostname allowlist with wildcard matching (Phase 3b of #934).
//!
//! Used by the built-in HTTP CONNECT proxy ([`super::server`], TODO) to
//! decide whether an incoming `CONNECT host:port` request should be
//! tunneled or rejected with `403`.
//!
//! ## Match semantics
//!
//! Patterns come in two flavors:
//!
//! - **Exact** — `github.com` matches `github.com`, case-insensitive.
//! - **Wildcard** — `*.npmjs.org` matches `registry.npmjs.org`,
//!   `www.npmjs.org`, **but not** `npmjs.org` (the bare apex) or
//!   `evil.npmjs.org.attacker.com` (suffix-based phishing).
//!
//! Wildcard rules borrowed from RFC 6125 (TLS Common Name):
//!
//! - Wildcard appears **only** as the leftmost label and **only** as
//!   `*.` followed by at least one label.
//! - Wildcard matches **exactly one** label (no nested subdomains).
//!   So `*.npmjs.org` matches `a.npmjs.org` but **not** `a.b.npmjs.org`.
//! - Patterns like `*foo.com`, `foo.*`, or bare `*` are rejected by
//!   [`Filter::new`] (returns an error).
//!
//! ## Why one-label wildcards (and not greedy)?
//!
//! Greedy wildcards (`*.npmjs.org` matching `a.b.npmjs.org`) are subtly
//! dangerous: a misconfigured CDN that lets attackers register
//! `evil.cdn.npmjs.org` would also match. RFC 6125 chose single-label
//! wildcards to bound this blast radius; we follow suit. Users who need
//! deeper matching can add `*.cdn.npmjs.org` explicitly — boring and
//! auditable.
//!
//! ## Why no IDN normalization?
//!
//! Patterns are byte-compared (after ASCII lowercasing). Users wanting
//! to allow `bücher.de` must pre-encode it as `xn--bcher-kva.de`. Same
//! contract as TLS certificate matching. Saves a `idna` dep and makes
//! the matcher branch-free.
//!
//! ## Default allowlist
//!
//! [`DEFAULT_DEV_ALLOWLIST`] ships a tiny set of dev-ecosystem hosts
//! (GitHub, npmjs, PyPI, crates.io, Docker Hub) that almost every
//! engineering session needs. Used by `koda-core` when user config is
//! silent. Empty if the user explicitly sets `net.allowlist = []`.

use anyhow::{Result, bail};

/// Tiny dev-ecosystem allowlist used as the default seed by `koda-core`
/// when the user hasn't configured `net.allowlist`.
///
/// Intentionally small — anything beyond these gets configured per-project.
/// No corporate / vendor domains baked in (those belong in user config).
pub const DEFAULT_DEV_ALLOWLIST: &[&str] = &[
    // Source code
    "github.com",
    "*.githubusercontent.com",
    "api.github.com",
    "codeload.github.com",
    // Package registries
    "*.npmjs.org",
    "registry.npmjs.org",
    "*.pypi.org",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    // Container registries
    "*.docker.io",
    "registry-1.docker.io",
    "auth.docker.io",
];

/// One pattern in the allowlist. Two shapes only — exact or wildcard.
///
/// Internal type. Construct via [`Filter::new`] which validates the
/// surface syntax; building one of these by hand bypasses validation
/// and isn't supported.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pattern {
    /// Exact host match, case-insensitive. Stored lowercased.
    Exact(String),
    /// Single-label wildcard. The stored string is the suffix
    /// **after** the leading `*` (e.g. `*.npmjs.org` → `.npmjs.org`).
    /// Match logic: incoming host must end with this suffix AND the
    /// part before the suffix must be a single non-empty label
    /// (no dots).
    Wildcard(String),
}

/// Allowlist filter for the egress proxy.
///
/// Cheap to clone (just a `Vec<Pattern>`); cheap to query (linear
/// scan over a typically-tiny pattern list, with early exit). For
/// the expected size (5–50 patterns) the constant factor of a
/// hash-based or trie-based structure isn't worth it.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    patterns: Vec<Pattern>,
}

impl Filter {
    /// Build a filter from a list of patterns.
    ///
    /// Returns an error if any pattern violates the wildcard syntax
    /// (see [module docs](self) for the rules).
    pub fn new<I, S>(patterns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut out = Vec::new();
        for raw in patterns {
            out.push(parse_pattern(raw.as_ref())?);
        }
        Ok(Self { patterns: out })
    }

    /// `true` iff `host` matches at least one pattern.
    ///
    /// `host` may include a `:port` suffix — it's stripped before
    /// matching. Hosts are lowercased for the comparison.
    pub fn allows(&self, host: &str) -> bool {
        let host = strip_port(host).to_ascii_lowercase();
        for p in &self.patterns {
            match p {
                Pattern::Exact(want) => {
                    if host == *want {
                        return true;
                    }
                }
                Pattern::Wildcard(suffix) => {
                    if let Some(prefix) = host.strip_suffix(suffix) {
                        // Single-label rule: the prefix must be non-empty
                        // and contain no dots.
                        if !prefix.is_empty() && !prefix.contains('.') {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Number of patterns. Useful for `tracing` and tests.
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// Whether the filter has zero patterns (deny-all).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

/// Strip a `:port` suffix if present. Loopback IPv6 (`[::1]:443`) is
/// out of scope — those go through `NO_PROXY` and never hit us.
fn strip_port(host: &str) -> &str {
    match host.rsplit_once(':') {
        Some((h, _)) => h,
        None => host,
    }
}

/// Parse + validate one pattern string.
///
/// See module docs for the rules. Pulled out of [`Filter::new`] so
/// the validation has a single tested code path independent of the
/// iterator-of-strings entry point.
fn parse_pattern(raw: &str) -> Result<Pattern> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("pattern must not be empty");
    }
    let lower = raw.to_ascii_lowercase();

    if let Some(rest) = lower.strip_prefix("*.") {
        // Wildcard. Validate the rest contains no further wildcards
        // and at least one label.
        if rest.is_empty() {
            bail!("pattern {raw:?}: wildcard must be followed by at least one label");
        }
        if rest.contains('*') {
            bail!("pattern {raw:?}: wildcard may only appear as the leftmost label");
        }
        // Build the matchable suffix including the leading dot — the
        // dot is what enforces the label boundary in `allows()`.
        Ok(Pattern::Wildcard(format!(".{rest}")))
    } else {
        if lower.contains('*') {
            bail!("pattern {raw:?}: wildcard only allowed as leftmost label, e.g. *.example.com");
        }
        Ok(Pattern::Exact(lower))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_pattern ────────────────────────────────────────────────────

    #[test]
    fn parse_pattern_accepts_exact() {
        assert_eq!(
            parse_pattern("github.com").unwrap(),
            Pattern::Exact("github.com".into())
        );
    }

    #[test]
    fn parse_pattern_lowercases_exact() {
        assert_eq!(
            parse_pattern("GitHub.COM").unwrap(),
            Pattern::Exact("github.com".into())
        );
    }

    #[test]
    fn parse_pattern_accepts_wildcard() {
        assert_eq!(
            parse_pattern("*.npmjs.org").unwrap(),
            Pattern::Wildcard(".npmjs.org".into())
        );
    }

    #[test]
    fn parse_pattern_lowercases_wildcard() {
        assert_eq!(
            parse_pattern("*.NPMJS.org").unwrap(),
            Pattern::Wildcard(".npmjs.org".into())
        );
    }

    #[test]
    fn parse_pattern_trims_whitespace() {
        assert_eq!(
            parse_pattern("  github.com  ").unwrap(),
            Pattern::Exact("github.com".into())
        );
    }

    #[test]
    fn parse_pattern_rejects_empty() {
        let err = parse_pattern("").expect_err("must reject empty");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_pattern_rejects_whitespace_only() {
        let err = parse_pattern("   ").expect_err("must reject whitespace-only");
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_pattern_rejects_bare_wildcard() {
        let err = parse_pattern("*.").expect_err("must reject bare *.");
        assert!(err.to_string().contains("at least one label"));
    }

    #[test]
    fn parse_pattern_rejects_internal_wildcard() {
        let err = parse_pattern("foo.*.com").expect_err("must reject internal *");
        assert!(err.to_string().contains("leftmost label"));
    }

    #[test]
    fn parse_pattern_rejects_trailing_wildcard() {
        let err = parse_pattern("foo.*").expect_err("must reject trailing *");
        assert!(err.to_string().contains("leftmost label"));
    }

    #[test]
    fn parse_pattern_rejects_double_wildcard() {
        let err = parse_pattern("*.*.com").expect_err("must reject *.*");
        assert!(err.to_string().contains("leftmost label"));
    }

    #[test]
    fn parse_pattern_rejects_prefix_glob() {
        // "*foo.com" — wildcard not as a full leftmost label.
        let err = parse_pattern("*foo.com").expect_err("must reject *foo");
        assert!(err.to_string().contains("leftmost label"));
    }

    // ── Filter::allows: exact ───────────────────────────────────────────

    #[test]
    fn allows_exact_match() {
        let f = Filter::new(["github.com"]).unwrap();
        assert!(f.allows("github.com"));
    }

    #[test]
    fn allows_exact_match_case_insensitive() {
        let f = Filter::new(["github.com"]).unwrap();
        assert!(f.allows("GitHub.COM"));
    }

    #[test]
    fn allows_exact_strips_port() {
        let f = Filter::new(["github.com"]).unwrap();
        assert!(f.allows("github.com:443"));
    }

    #[test]
    fn allows_exact_does_not_match_subdomain() {
        let f = Filter::new(["github.com"]).unwrap();
        assert!(!f.allows("api.github.com"));
    }

    #[test]
    fn allows_exact_does_not_match_suffix() {
        // Classic phishing vector: "evilgithub.com" — must not match.
        let f = Filter::new(["github.com"]).unwrap();
        assert!(!f.allows("evilgithub.com"));
    }

    // ── Filter::allows: wildcard ────────────────────────────────────────

    #[test]
    fn allows_wildcard_matches_one_subdomain() {
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(f.allows("registry.npmjs.org"));
        assert!(f.allows("www.npmjs.org"));
    }

    #[test]
    fn allows_wildcard_does_not_match_apex() {
        // RFC 6125: *.npmjs.org must not match the bare apex "npmjs.org".
        // Users wanting the apex add it explicitly.
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(!f.allows("npmjs.org"));
    }

    #[test]
    fn allows_wildcard_does_not_match_two_labels() {
        // RFC 6125: wildcard matches exactly one label. *.npmjs.org
        // does NOT match a.b.npmjs.org.
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(!f.allows("a.b.npmjs.org"));
    }

    #[test]
    fn allows_wildcard_does_not_match_suffix_attack() {
        // Suffix phishing: "evil.npmjs.org.attacker.com" must not match
        // *.npmjs.org. The label-boundary check (no dot before the
        // suffix dot) is what saves us here.
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(!f.allows("evil.npmjs.org.attacker.com"));
    }

    #[test]
    fn allows_wildcard_strips_port() {
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(f.allows("registry.npmjs.org:443"));
    }

    #[test]
    fn allows_wildcard_case_insensitive() {
        let f = Filter::new(["*.npmjs.org"]).unwrap();
        assert!(f.allows("REGISTRY.NPMJS.ORG"));
    }

    // ── Filter::allows: combinations ────────────────────────────────────

    #[test]
    fn allows_apex_and_wildcard_together() {
        // Common pattern: allow both apex and subdomains by listing both.
        let f = Filter::new(["pypi.org", "*.pypi.org"]).unwrap();
        assert!(f.allows("pypi.org"));
        assert!(f.allows("files.pypi.org"));
    }

    #[test]
    fn allows_returns_false_when_empty() {
        // Default filter is deny-all (no patterns).
        let f = Filter::default();
        assert!(!f.allows("github.com"));
        assert!(!f.allows("anything.example.com"));
    }

    #[test]
    fn allows_short_circuits_on_first_match() {
        // Behavioral: once any pattern matches we return true.
        // Implementation guard against future "all must match" regressions.
        let f = Filter::new(["github.com", "*.npmjs.org"]).unwrap();
        assert!(f.allows("github.com"));
        assert!(f.allows("registry.npmjs.org"));
    }

    // ── DEFAULT_DEV_ALLOWLIST ───────────────────────────────────────────

    #[test]
    fn default_dev_allowlist_parses_cleanly() {
        // Regression guard: no typos sneaking into the default list.
        let f = Filter::new(DEFAULT_DEV_ALLOWLIST).expect("default allowlist must parse");
        assert_eq!(f.len(), DEFAULT_DEV_ALLOWLIST.len());
    }

    #[test]
    fn default_dev_allowlist_covers_common_cases() {
        let f = Filter::new(DEFAULT_DEV_ALLOWLIST).unwrap();
        // The hosts every dev tool reaches for.
        assert!(f.allows("github.com"));
        assert!(f.allows("api.github.com"));
        assert!(f.allows("registry.npmjs.org"));
        assert!(f.allows("pypi.org"));
        assert!(f.allows("files.pythonhosted.org"));
        assert!(f.allows("crates.io"));
        assert!(f.allows("registry-1.docker.io"));
        // And the obvious negatives.
        assert!(!f.allows("evil.example.com"));
        assert!(!f.allows("nation-state.adversary.io"));
    }

    // ── Misc ────────────────────────────────────────────────────────────

    #[test]
    fn len_and_is_empty() {
        let f = Filter::default();
        assert_eq!(f.len(), 0);
        assert!(f.is_empty());

        let f = Filter::new(["a.com", "b.com"]).unwrap();
        assert_eq!(f.len(), 2);
        assert!(!f.is_empty());
    }

    #[test]
    fn strip_port_handles_no_port() {
        assert_eq!(strip_port("github.com"), "github.com");
    }

    #[test]
    fn strip_port_handles_port() {
        assert_eq!(strip_port("github.com:443"), "github.com");
    }
}
