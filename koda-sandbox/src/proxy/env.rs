//! Env-var bouquet for routing sandboxed subprocesses through the proxy
//! (Phase 3a of #934).
//!
//! See [parent module docs](super) for the why.

use std::path::Path;

/// Default `NO_PROXY` value: loopback + RFC1918 + AWS/GCE IMDS.
///
/// Borrowed verbatim from Claude Code's `upstreamproxy.ts` `NO_PROXY_LIST`
/// (which itself mirrors the Bun/curl/Go/Python intersection of supported
/// patterns). These are the addresses every reasonable proxy declines to
/// intercept — without them, sandboxed processes can't talk to localhost
/// dev servers, can't read instance metadata, and can't reach RFC1918
/// services on the user's LAN.
pub const DEFAULT_NO_PROXY: &str =
    "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";

/// Env-var name passed to a user proxy command so it knows which port to bind.
///
/// Mirrors Codex's `PROXY_ACTIVE_ENV_KEY` pattern. Avoids template-string
/// parsing and lets the proxy command be a plain `Vec<String>`.
pub const PROXY_PORT_ENV_KEY: &str = "KODA_PROXY_PORT";

/// Generate the env-var bouquet for a sandboxed subprocess.
///
/// `port` is where the proxy is listening on `127.0.0.1`. `ca_bundle` is
/// the path to a PEM bundle the subprocess should trust for TLS verification
/// — typically points to a corporate CA + system CA concatenation. When
/// `None`, the cert-bundle vars are omitted (the subprocess uses its
/// platform default trust store).
///
/// Returned as `Vec<(String, String)>` so the caller can `.envs(...)` it
/// directly into a `Command` builder. Sorted by key for deterministic
/// snapshot tests.
///
/// ## Why so many keys?
///
/// Different runtimes look at different env vars:
///
/// | Runtime | Proxy var | CA bundle var |
/// |---|---|---|
/// | curl, libcurl | `HTTPS_PROXY` (UPPER) | `CURL_CA_BUNDLE` |
/// | Python `requests` | `HTTPS_PROXY` (UPPER) | `REQUESTS_CA_BUNDLE` |
/// | Python `httpx`, `urllib` | `https_proxy` (lower) | `SSL_CERT_FILE` |
/// | Node.js (undici) | `HTTPS_PROXY` (UPPER) | `NODE_EXTRA_CA_CERTS` |
/// | Go (`net/http`) | `HTTPS_PROXY` (UPPER) | `SSL_CERT_FILE` |
/// | Rust (`reqwest`) | `HTTPS_PROXY` (UPPER) | `SSL_CERT_FILE` |
///
/// Setting all of them is the only way to cover every dev tool without
/// per-tool wrappers.
pub fn proxy_env_vars(port: u16, ca_bundle: Option<&Path>) -> Vec<(String, String)> {
    let proxy_url = format!("http://127.0.0.1:{port}");

    let mut vars = vec![
        ("HTTPS_PROXY".to_string(), proxy_url.clone()),
        ("https_proxy".to_string(), proxy_url.clone()),
        ("HTTP_PROXY".to_string(), proxy_url.clone()),
        ("http_proxy".to_string(), proxy_url),
        ("NO_PROXY".to_string(), DEFAULT_NO_PROXY.to_string()),
        ("no_proxy".to_string(), DEFAULT_NO_PROXY.to_string()),
    ];

    if let Some(ca) = ca_bundle {
        let ca_str = ca.to_string_lossy().to_string();
        vars.push(("SSL_CERT_FILE".to_string(), ca_str.clone()));
        vars.push(("NODE_EXTRA_CA_CERTS".to_string(), ca_str.clone()));
        vars.push(("REQUESTS_CA_BUNDLE".to_string(), ca_str.clone()));
        vars.push(("CURL_CA_BUNDLE".to_string(), ca_str));
    }

    // Deterministic order for snapshot tests.
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

/// Env-var bouquet for the SOCKS5 proxy: `ALL_PROXY` + lowercase
/// alias, both pointing at `socks5h://127.0.0.1:port`.
///
/// The `socks5h://` (vs `socks5://`) scheme is **not optional** — it
/// instructs the client to forward the hostname to the proxy for
/// resolution rather than pre-resolving locally and sending an IP
/// literal. The built-in SOCKS5 server [refuses IP literals]
/// (super::socks5#subset) precisely because they bypass the hostname
/// allow list. Mismatched schemes here would silently break SOCKS5
/// for every client that obeys `ALL_PROXY`.
///
/// Returned as a separate `Vec` (rather than appended to
/// [`proxy_env_vars`]) so callers can opt into SOCKS5 independently —
/// not every session needs it, and a sandboxed shell that only ever
/// runs `curl` doesn't benefit from one extra env knob to debug.
pub fn socks5_env_vars(port: u16) -> Vec<(String, String)> {
    let url = format!("socks5h://127.0.0.1:{port}");
    let mut vars = vec![
        ("ALL_PROXY".to_string(), url.clone()),
        ("all_proxy".to_string(), url),
    ];
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    vars
}

/// Path to the CA bundle to advertise via env vars, given a [`crate::policy::NetPolicy`].
///
/// Returns `None` when no MITM is configured — in that case the subprocess
/// uses its platform default trust store. Returning `Option<&Path>` rather
/// than `Option<PathBuf>` so callers can pass it directly to [`proxy_env_vars`]
/// without intermediate allocation.
pub fn ca_bundle_for_policy(net: &crate::policy::NetPolicy) -> Option<&Path> {
    net.mitm.as_ref().map(|m| m.ca_bundle.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn proxy_env_vars_includes_all_six_proxy_keys() {
        let vars = proxy_env_vars(8080, None);
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(keys.contains(&"HTTPS_PROXY"));
        assert!(keys.contains(&"https_proxy"));
        assert!(keys.contains(&"HTTP_PROXY"));
        assert!(keys.contains(&"http_proxy"));
        assert!(keys.contains(&"NO_PROXY"));
        assert!(keys.contains(&"no_proxy"));
    }

    #[test]
    fn proxy_env_vars_omits_ca_bundle_when_none() {
        let vars = proxy_env_vars(8080, None);
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(!keys.contains(&"SSL_CERT_FILE"));
        assert!(!keys.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(!keys.contains(&"REQUESTS_CA_BUNDLE"));
        assert!(!keys.contains(&"CURL_CA_BUNDLE"));
    }

    #[test]
    fn proxy_env_vars_includes_all_four_ca_keys_when_some() {
        let bundle = PathBuf::from("/etc/ssl/corp-ca.pem");
        let vars = proxy_env_vars(8080, Some(&bundle));
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();

        assert!(keys.contains(&"SSL_CERT_FILE"));
        assert!(keys.contains(&"NODE_EXTRA_CA_CERTS"));
        assert!(keys.contains(&"REQUESTS_CA_BUNDLE"));
        assert!(keys.contains(&"CURL_CA_BUNDLE"));

        // All four point at the same path.
        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
        ] {
            let v = vars
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap();
            assert_eq!(v, "/etc/ssl/corp-ca.pem");
        }
    }

    #[test]
    fn proxy_url_format_uses_loopback_ipv4() {
        let vars = proxy_env_vars(31415, None);
        let url = vars
            .iter()
            .find(|(k, _)| k == "HTTPS_PROXY")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(url, "http://127.0.0.1:31415");
    }

    #[test]
    fn no_proxy_default_covers_loopback_and_rfc1918() {
        // Sanity check that we haven't accidentally truncated the constant.
        assert!(DEFAULT_NO_PROXY.contains("127.0.0.1"));
        assert!(DEFAULT_NO_PROXY.contains("::1"));
        assert!(DEFAULT_NO_PROXY.contains("10.0.0.0/8"));
        assert!(DEFAULT_NO_PROXY.contains("172.16.0.0/12"));
        assert!(DEFAULT_NO_PROXY.contains("192.168.0.0/16"));
        // AWS / GCE IMDS link-local: dropping this would prevent cloud
        // workloads from reading instance metadata.
        assert!(DEFAULT_NO_PROXY.contains("169.254.0.0/16"));
    }

    #[test]
    fn socks5_env_vars_uses_socks5h_scheme() {
        // The `h` matters — see fn docs.
        let vars = socks5_env_vars(1080);
        for (_, v) in &vars {
            assert!(v.starts_with("socks5h://"), "got: {v}");
        }
    }

    #[test]
    fn socks5_env_vars_includes_upper_and_lower() {
        let vars = socks5_env_vars(1080);
        let keys: Vec<&str> = vars.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"ALL_PROXY"));
        assert!(keys.contains(&"all_proxy"));
    }

    #[test]
    fn socks5_env_vars_uses_loopback_ipv4() {
        let vars = socks5_env_vars(31415);
        let url = vars
            .iter()
            .find(|(k, _)| k == "ALL_PROXY")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(url, "socks5h://127.0.0.1:31415");
    }

    #[test]
    fn ca_bundle_for_policy_handles_no_mitm() {
        let policy = crate::policy::NetPolicy::default();
        assert!(ca_bundle_for_policy(&policy).is_none());
    }

    #[test]
    fn ca_bundle_for_policy_returns_path_when_mitm_set() {
        let policy = crate::policy::NetPolicy {
            mitm: Some(crate::policy::MitmConfig {
                ca_bundle: PathBuf::from("/x/ca.pem"),
                socket_map: vec![],
            }),
            ..Default::default()
        };
        assert_eq!(ca_bundle_for_policy(&policy), Some(Path::new("/x/ca.pem")));
    }

    /// Phase 3g of #934: full pipeline from a policy with `MitmConfig`
    /// to the env bouquet that lands in the sandboxed subprocess.
    /// Composes `ca_bundle_for_policy` and `proxy_env_vars` exactly
    /// the way `worker_client.rs` and `koda-core/src/sandbox.rs` do
    /// at runtime — catches a regression where a future refactor
    /// detaches the two halves and silently sends `None` again.
    #[test]
    fn policy_with_mitm_yields_full_ca_bouquet() {
        let policy = crate::policy::NetPolicy {
            mitm: Some(crate::policy::MitmConfig {
                ca_bundle: PathBuf::from("/etc/ssl/corp.pem"),
                socket_map: vec![],
            }),
            ..Default::default()
        };
        let ca = ca_bundle_for_policy(&policy);
        let vars = proxy_env_vars(8080, ca);

        // All four CA-bundle keys must be present and point at the
        // same path — different runtimes look at different keys (see
        // the table in `proxy_env_vars` doc), and a partial bouquet
        // means some tools silently bypass the corp-CA verification.
        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
        ] {
            let v = vars
                .iter()
                .find(|(k, _)| k == key)
                .unwrap_or_else(|| panic!("missing {key} in {vars:?}"));
            assert_eq!(
                v.1, "/etc/ssl/corp.pem",
                "{key} must point at the policy's ca_bundle"
            );
        }
    }

    /// Negative companion: the default policy (no MITM configured)
    /// must NOT inject any CA env vars — doing so would override the
    /// platform default trust store with an empty path and break
    /// every TLS handshake. Same composition shape as the positive
    /// case to make the diff between them obvious.
    #[test]
    fn policy_without_mitm_yields_no_ca_bouquet() {
        let policy = crate::policy::NetPolicy::default();
        let ca = ca_bundle_for_policy(&policy);
        let vars = proxy_env_vars(8080, ca);

        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "REQUESTS_CA_BUNDLE",
            "CURL_CA_BUNDLE",
        ] {
            assert!(
                !vars.iter().any(|(k, _)| k == key),
                "default policy must not advertise {key}; got {vars:?}"
            );
        }
    }
}
