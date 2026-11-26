//! Whether a server's version is this client's.
//!
//! Every Koda client and the shared server run the same version (Lijun,
//! 2026-11-26): Koda doesn't keep the protocol backward compatible, so a
//! client uses only a server of its own version. A client that finds an
//! older server replaces it (`serve/stop` with `forUpdate`); one that finds
//! a newer server says Koda was updated and doesn't use it. Codex's TUI
//! uses a server of another version and warns when it is older; Claude
//! Code warns too. This is stricter on purpose.

/// How a server's version stands against this client's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerVersion {
    /// The same: use it.
    Same,
    /// Older, or another build of the same release: replace it.
    Older,
    /// Newer: Koda was updated since this client started.
    Newer,
}

/// Compare `server`'s version with `client`'s, by release number
/// (`major.minor.patch`); a server whose release can't be read is older.
pub fn compare(server: &str, client: &str) -> ServerVersion {
    if server == client {
        return ServerVersion::Same;
    }
    match (release(server), release(client)) {
        (Some(server), Some(client)) if server > client => ServerVersion::Newer,
        _ => ServerVersion::Older,
    }
}

/// The `major.minor.patch` that leads `version`, without a pre-release or
/// build suffix.
fn release(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let release = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_the_same_older_or_newer_by_release() {
        assert_eq!(compare("0.7.13", "0.7.13"), ServerVersion::Same);
        assert_eq!(compare("0.7.12", "0.7.13"), ServerVersion::Older);
        assert_eq!(compare("0.7.14", "0.7.13"), ServerVersion::Newer);
        assert_eq!(compare("0.10.0", "0.9.9"), ServerVersion::Newer);
        assert_eq!(compare("1.0.0", "0.99.0"), ServerVersion::Newer);
        // Another build of the same release is replaced, never kept.
        assert_eq!(compare("0.7.13-dev", "0.7.13"), ServerVersion::Older);
        assert_eq!(compare("0.7.13", "0.7.13+local"), ServerVersion::Older);
        assert_eq!(compare("0.8.0-rc.1", "0.7.13"), ServerVersion::Newer);
        // What can't be read is replaced.
        assert_eq!(compare("koda", "0.7.13"), ServerVersion::Older);
        assert_eq!(compare("0.7", "0.7.13"), ServerVersion::Older);
    }
}
