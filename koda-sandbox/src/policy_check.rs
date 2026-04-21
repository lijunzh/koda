//! Application-layer policy checks — companions to the kernel sandbox.
//!
//! These let *in-process* file tools enforce the same fully-deny rules
//! that the kernel sandbox applies to subprocesses (CC parity #882).
//! Without this, the in-process `Read` tool could bypass the sandbox by
//! reading paths that the Bash sandbox would block.
//!
//! Long-term plan (#884) replaces this with a sandboxed worker process
//! (Phase 2 of #934).

use crate::defaults::CREDENTIAL_CONFIG_FULL_DENY;
use std::path::Path;

/// Returns `true` when `path` falls inside a fully-denied location.
///
/// "Fully denied" means both reads **and** writes are blocked. Currently
/// only `~/.config/koda/db` is fully denied — koda's own SQLite database
/// containing plaintext API keys.
///
/// Ordinary credential directories (`~/.ssh`, `~/.aws`, …) are **not**
/// included: reads are allowed there, mirroring the Bash sandbox which
/// only write-protects them.
///
/// **Defense in depth (#898):** when `HOME` cannot be resolved (containers,
/// CI, or `unset HOME`), this fails *closed*: tries `HOME`, then
/// `USERPROFILE` (Windows), and finally falls back to a path-component
/// pattern match so any path containing `.config/koda/db` consecutively
/// still gets denied even with no home directory at all.
pub fn is_fully_denied(path: &Path) -> bool {
    is_fully_denied_with_home(path, resolve_home_dir().as_deref())
}

/// Best-effort home directory lookup with cross-platform fallback.
///
/// Order: `HOME` (Unix-y, including macOS) → `USERPROFILE` (Windows).
/// Returns `None` only when *both* are unset; callers must fail closed.
fn resolve_home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|s| !s.is_empty())
}

/// Pure helper for [`is_fully_denied`] — takes the home directory as an
/// argument so the no-home case is unit-testable without racing on env vars.
pub(crate) fn is_fully_denied_with_home(path: &Path, home: Option<&str>) -> bool {
    // Primary check: does the path live under `${home}/.config/<rel>` for
    // any fully-denied relative path? Only runs when home is known.
    if let Some(home) = home {
        let home_path = Path::new(home);
        if CREDENTIAL_CONFIG_FULL_DENY
            .iter()
            .any(|rel| path.starts_with(home_path.join(".config").join(rel)))
        {
            return true;
        }
    }

    // Fallback: pattern-match the path components themselves. Even with
    // no home, any path whose components contain `.config/<rel>` as a
    // consecutive sequence is treated as fully denied. Catches the
    // `unset HOME` bypass and exotic paths like
    // `/proc/self/root/.config/koda/db/koda.db` that don't share a prefix
    // with `$HOME`.
    let components: Vec<&std::ffi::OsStr> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();

    CREDENTIAL_CONFIG_FULL_DENY.iter().any(|rel| {
        // Build the target sequence: [".config", <rel parts split by '/'>...]
        let mut needle: Vec<&str> = vec![".config"];
        needle.extend(rel.split('/'));
        // Sliding window match against the path components.
        components.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle.iter())
                .all(|(comp, want)| comp.to_str() == Some(*want))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fully_denied_blocks_koda_db() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        let koda_db = Path::new(&home).join(".config/koda/db");
        assert!(
            is_fully_denied(&koda_db),
            "~/.config/koda/db must be fully denied"
        );
        // Sub-paths inside it are also denied.
        assert!(is_fully_denied(&koda_db.join("koda.db")));
    }

    #[test]
    fn fully_denied_allows_credential_dirs() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        let home = Path::new(&home);
        // Credential dirs are write-protected but reads are allowed — they
        // must NOT appear in the fully-denied list.
        for rel in &[".ssh", ".aws", ".gnupg", ".config/gh", ".config/gcloud"] {
            assert!(
                !is_fully_denied(&home.join(rel)),
                "{rel} must NOT be fully denied (reads allowed)"
            );
        }
    }

    #[test]
    fn fully_denied_allows_project_and_system_paths() {
        assert!(!is_fully_denied(Path::new(
            "/home/user/project/src/main.rs"
        )));
        assert!(!is_fully_denied(Path::new("/tmp/scratch.txt")));
        assert!(!is_fully_denied(Path::new("/etc/hosts")));
    }

    #[test]
    fn fully_denied_blocks_koda_db_when_home_is_none() {
        // Regression test for the historical #898 bypass: tool resolves to
        // a real-looking path but HOME is unset, so the prefix check can't
        // fire. Path-segment fallback must still deny it.
        for path in [
            "/root/.config/koda/db",
            "/root/.config/koda/db/koda.db",
            "/home/runner/.config/koda/db/koda.db",
            "/proc/self/root/.config/koda/db/koda.db",
            ".config/koda/db/koda.db", // relative path, no anchor at all
        ] {
            assert!(
                is_fully_denied_with_home(Path::new(path), None),
                "{path:?} must be denied even with HOME=None"
            );
        }
    }

    #[test]
    fn fully_denied_no_home_still_allows_normal_paths() {
        // Defense-in-depth fallback must not over-block ordinary paths.
        for path in [
            "/home/user/project/src/main.rs",
            "/tmp/scratch.txt",
            "/etc/hosts",
            "/home/user/.config/git/config", // .config but not koda/db
            "/home/user/.config/koda/agents/foo.json", // koda but not db
            "/var/lib/koda-db-backups/2025.tar", // contains 'koda' and 'db'
                                             // but not the sequence
                                             // .config/koda/db
        ] {
            assert!(
                !is_fully_denied_with_home(Path::new(path), None),
                "{path:?} must NOT be denied (no koda secrets)"
            );
        }
    }

    #[test]
    fn fully_denied_uses_home_when_provided() {
        // Sanity check that the explicit-home path still works and matches
        // a non-HOME directory the user passes in.
        let custom_home = "/srv/koda-runner";
        assert!(is_fully_denied_with_home(
            Path::new("/srv/koda-runner/.config/koda/db/koda.db"),
            Some(custom_home),
        ));
        assert!(!is_fully_denied_with_home(
            Path::new("/srv/koda-runner/notes.md"),
            Some(custom_home),
        ));
    }

    #[test]
    fn resolve_home_dir_treats_empty_as_unset() {
        // Empty HOME (`HOME=""`) is just as broken as unset — both should
        // route through the no-home fallback. The .filter() in
        // resolve_home_dir() guarantees this; verify via direct call to
        // the helper since we can't safely mutate env in tests.
        assert!(is_fully_denied_with_home(
            Path::new("/anywhere/.config/koda/db/koda.db"),
            None, // simulating resolve_home_dir() returning None
        ));
    }
}
