//! Which addresses an outbound request may go to: not loopback, private,
//! link-local or cloud-metadata ones, so content the model fetches can't
//! steer a request into the machine's own network (server-side request
//! forgery). Web fetch checks a URL and each address it resolves to; an MCP
//! server's HTTP URL is checked too.

/// Check if an IP address is safe (not private/internal/loopback).
pub fn is_safe_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            // Loopback, private, link-local, unspecified
            let internal = octets[0] == 127
                || octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
                || ipv4.is_unspecified();
            !internal
        }
        std::net::IpAddr::V6(ipv6) => {
            if ipv6.is_loopback() || ipv6.is_unspecified() {
                return false;
            }
            if let Some(ipv4) = ipv6.to_ipv4_mapped() {
                return is_safe_ip(std::net::IpAddr::V4(ipv4));
            }
            true
        }
    }
}

/// Check if a URL is safe to fetch (not internal/private network).
/// Uses the `url` crate for robust parsing (handles userinfo@, IPv6, etc.).
pub fn is_safe_url(url_str: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };

    // Block known metadata hostnames
    let blocked_hosts = [
        "169.254.169.254",
        "metadata.google.internal",
        "metadata.internal",
        "localhost",
        "0.0.0.0",
    ];
    if blocked_hosts.contains(&host) {
        return false;
    }

    // Block .internal and .local TLDs
    if host.ends_with(".internal") || host.ends_with(".local") {
        return false;
    }

    // Block private/reserved IPs using the parsed host
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if !is_safe_ip(std::net::IpAddr::V4(ip)) {
                return false;
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if !is_safe_ip(std::net::IpAddr::V6(ip)) {
                return false;
            }
        }
        Some(url::Host::Domain(_)) => {
            // Domain names — hostname checks above are sufficient
            // (DNS resolution check happens separately in web_fetch)
        }
        None => return false,
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_safe_url_blocks_metadata() {
        assert!(!is_safe_url("http://169.254.169.254/latest/meta-data/"));
        assert!(!is_safe_url("http://metadata.google.internal/"));
    }

    #[test]
    fn test_is_safe_url_blocks_localhost() {
        assert!(!is_safe_url("http://localhost:8080/admin"));
        assert!(!is_safe_url("http://127.0.0.1/secret"));
        assert!(!is_safe_url("http://0.0.0.0/"));
    }

    #[test]
    fn test_is_safe_url_blocks_private_ips() {
        assert!(!is_safe_url("http://10.0.0.1/internal"));
        assert!(!is_safe_url("http://172.16.0.1/admin"));
        assert!(!is_safe_url("http://192.168.1.1/config"));
    }

    #[test]
    fn test_is_safe_url_blocks_userinfo_bypass() {
        // RFC 3986 userinfo@ component should not fool the parser
        assert!(!is_safe_url(
            "http://evil.com@169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_url("http://user:pass@127.0.0.1/"));
    }

    #[test]
    fn test_is_safe_url_blocks_ipv6_mapped() {
        assert!(!is_safe_url("http://[::ffff:127.0.0.1]/"));
        assert!(!is_safe_url("http://[::1]/"));
    }

    #[test]
    fn test_is_safe_url_allows_public() {
        assert!(is_safe_url("https://docs.rs/tokio/latest/tokio/"));
        assert!(is_safe_url("https://api.github.com/repos"));
        assert!(is_safe_url("https://example.com"));
    }

    // ── is_safe_ip tests (#526) ──

    #[test]
    fn test_is_safe_ip_blocks_private() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(!is_safe_ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
        assert!(!is_safe_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_safe_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn test_is_safe_ip_allows_public() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_safe_ip(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
    }
}
