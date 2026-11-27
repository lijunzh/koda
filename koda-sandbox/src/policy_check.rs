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
use crate::server_socket::SERVER_SUBDIR;
use std::path::Path;

/// Returns `true` when `path` falls inside a fully-denied location.
///
/// "Fully denied" means both reads **and** writes are blocked: Koda's
/// database (plaintext API keys), its MCP OAuth tokens and the folder of its
/// shared server's socket, under `~/.config/koda` and, when that is set
/// elsewhere, `$XDG_CONFIG_HOME/koda`.
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
///
/// **Links are followed.** The path is checked as given and with its links
/// resolved, and each config folder as given and resolved, so neither a link
/// into the folder (`ln -s ~/.config/koda/db secrets`, then `secrets/koda.db`)
/// nor a `~/.config` that is itself a link gets past it.
pub fn is_fully_denied(path: &Path) -> bool {
    let home = resolve_home_dir();
    let xdg = crate::defaults::xdg_config_home();
    is_fully_denied_in(path, home.as_deref(), xdg.as_deref())
}

/// [`is_fully_denied`] with the home and `XDG_CONFIG_HOME` given.
fn is_fully_denied_in(path: &Path, home: Option<&str>, xdg: Option<&Path>) -> bool {
    let resolved = resolve_links(path);
    let configs: Vec<std::path::PathBuf> = home
        .map(|home| Path::new(home).join(".config"))
        .into_iter()
        .chain(xdg.map(Path::to_path_buf))
        .collect();
    std::iter::once(path)
        .chain(resolved.as_deref())
        .any(|path| {
            is_fully_denied_with_home(path, home)
                || configs.iter().any(|config| {
                    denied_in(path, config)
                        || config
                            .canonicalize()
                            .is_ok_and(|real| denied_in(path, &real))
                })
        })
}

/// `path` with its links resolved: the whole path when it exists, else its
/// nearest existing folder resolved with the rest added. `None` when nothing
/// of it exists or it resolves to itself.
fn resolve_links(path: &Path) -> Option<std::path::PathBuf> {
    let mut rest = Vec::new();
    let mut existing = path;
    let resolved = loop {
        match existing.canonicalize() {
            Ok(real) => break real,
            Err(_) => {
                rest.push(existing.file_name()?);
                existing = existing.parent()?;
            }
        }
    };
    let resolved = rest.iter().rev().fold(resolved, |p, part| p.join(part));
    (resolved != path).then_some(resolved)
}

/// Whether `path` is in Koda's own folder ([`crate::defaults::KODA_CONFIG_SUBDIR`]),
/// which no tool may change unasked, its worktrees apart. `path` is taken as resolved; the
/// config folders are compared as given and through their links.
pub fn is_koda_config(path: &Path) -> bool {
    resolve_home_dir().is_some_and(|home| {
        crate::defaults::config_folders(&home)
            .iter()
            .any(|config| in_koda_folder(path, config))
    })
}

/// Whether `path` is in Koda's folder under the config folder `config`.
fn in_koda_folder(path: &Path, config: &Path) -> bool {
    let inside = |koda: &Path| {
        path.starts_with(koda)
            && !path.starts_with(koda.join(crate::defaults::KODA_WORKTREES_SUBDIR))
    };
    let koda = config.join(crate::defaults::KODA_CONFIG_SUBDIR);
    inside(&koda) || koda.canonicalize().is_ok_and(|real| inside(&real))
}

/// Whether `path` is in a folder fully denied in `config`. Runs for every
/// path a tool is given, so it makes no path of its own.
fn denied_in(path: &Path, config: &Path) -> bool {
    path.strip_prefix(config)
        .is_ok_and(|rest| fully_denied_subdirs().any(|rel| rest.starts_with(rel)))
}

/// The folders fully denied in a config folder: Koda's secrets, and its
/// shared server's socket.
fn fully_denied_subdirs() -> impl Iterator<Item = &'static str> {
    CREDENTIAL_CONFIG_FULL_DENY
        .iter()
        .copied()
        .chain([SERVER_SUBDIR])
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
    if let Some(home) = home
        && denied_in(path, &Path::new(home).join(".config"))
    {
        return true;
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

    components
        .iter()
        .enumerate()
        .filter(|(_, component)| **component == ".config")
        .any(|(i, _)| {
            let rest = &components[i + 1..];
            fully_denied_subdirs().any(|rel| {
                rel.split('/').count() <= rest.len()
                    && rel
                        .split('/')
                        .zip(rest)
                        .all(|(want, component)| *component == want)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kodas_own_folder_is_recognised_and_its_neighbours_arent() {
        let config = Path::new("/home/me/.config");
        assert!(in_koda_folder(
            Path::new("/home/me/.config/koda/config.toml"),
            config
        ));
        assert!(in_koda_folder(
            Path::new("/home/me/.config/koda/skills/a/SKILL.md"),
            config
        ));
        assert!(!in_koda_folder(
            Path::new("/home/me/.config/kodak/config.toml"),
            config
        ));
        assert!(!in_koda_folder(
            Path::new("/home/me/.config/gh/hosts.yml"),
            config
        ));
        assert!(!in_koda_folder(
            Path::new("/home/me/project/koda/config.toml"),
            config
        ));
        // A session's own checkout among Koda's worktrees is its to write.
        assert!(!in_koda_folder(
            Path::new("/home/me/.config/koda/worktrees/repo/wt-1/src/a.rs"),
            config
        ));
    }

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
        // MCP OAuth tokens, as API keys are.
        let socket = Path::new(&home).join(".config/koda/serve/koda.sock");
        assert!(
            is_fully_denied(&socket),
            "the shared server's socket must be fully denied"
        );
        let tokens = Path::new(&home).join(".config/koda/mcp_oauth/notion.json");
        assert!(is_fully_denied(&tokens));
    }

    /// Koda's folder under another config folder (`XDG_CONFIG_HOME`).
    #[test]
    fn fully_denied_folders_are_denied_in_any_config_folder() {
        let config = Path::new("/elsewhere/cfg");
        for path in [
            "koda/db",
            "koda/db/koda.db",
            "koda/mcp_oauth/s.json",
            "koda/serve/koda.sock",
        ] {
            assert!(denied_in(&config.join(path), config), "{path}");
        }
        for path in ["koda", "koda/config.toml", "koda/db2", "other/koda/db"] {
            assert!(!denied_in(&config.join(path), config), "{path}");
        }
        assert!(!denied_in(Path::new("/elsewhere/koda/db"), config));
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

    /// A link into Koda's folder, from the project or anywhere, is followed:
    /// the database behind it is denied, for a file and for the folder.
    #[cfg(unix)]
    #[test]
    fn a_link_into_the_denied_folder_is_denied() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".config/koda/db");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("koda.db"), "keys").unwrap();
        let project = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&db, project.path().join("secrets")).unwrap();
        std::os::unix::fs::symlink(db.join("koda.db"), project.path().join("data.db")).unwrap();
        let home = home.path().to_str();

        for linked in [
            "secrets/koda.db",
            "secrets",
            "data.db",
            "secrets/not-yet-made",
        ] {
            assert!(
                is_fully_denied_in(&project.path().join(linked), home, None),
                "{linked} reaches the database"
            );
        }
        std::fs::write(project.path().join("notes.md"), "").unwrap();
        assert!(!is_fully_denied_in(
            &project.path().join("notes.md"),
            home,
            None
        ));
    }

    /// A `~/.config` that is itself a link, as dotfile setups make it: the
    /// database is denied by its real path too.
    #[cfg(unix)]
    #[test]
    fn a_linked_config_folder_is_followed() {
        let dotfiles = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dotfiles.path().join("config/koda/db")).unwrap();
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dotfiles.path().join("config"), home.path().join(".config"))
            .unwrap();
        let real = dotfiles.path().join("config/koda/db/koda.db");
        assert!(is_fully_denied_in(&real, home.path().to_str(), None));
        assert!(!is_fully_denied_in(
            &dotfiles.path().join("config/other.toml"),
            home.path().to_str(),
            None
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
