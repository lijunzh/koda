//! Scratch paths outside the project: temp and cache directories that a
//! file tool may write without asking, as the kernel sandbox lets commands
//! write them.
//!
//! Commands aren't linted for paths: as in Codex, the kernel sandbox decides
//! what a command can write, and the dangerous-command check
//! ([`crate::bash_safety`]) what asks.

use std::path::{Path, PathBuf};

/// Whether `resolved` is a path that is safe to access outside the project root.
///
/// **Scratch-zone allowlist** (#1232 §8b): the user perceives writes to
/// these locations as scratchpad activity, not "escape from sandbox."
/// Hardcoded so the gate works even when `$TMPDIR` / `$XDG_CACHE_HOME`
/// aren't set in a sub-process's environment — which is exactly the
/// failure mode that drove the friction in the bug-review session
/// (sub-agents inherited a stripped env, lost `$TMPDIR`, fell through to
/// outside-project confirm prompts on every `/var/folders/.../T/...` write).
///
/// Safe paths include:
/// - Temp directories: `/tmp`, `$TMPDIR` (on macOS `/tmp` → `/private/tmp`,
///   `$TMPDIR` → `/private/var/folders/.../T/`)
/// - macOS scratch root: `/var/folders/` and `/private/var/folders/`
///   (env-independent fallback when `$TMPDIR` is missing)
/// - Koda cache: `~/.cache/koda/`, `$XDG_CACHE_HOME/koda/`
/// - Device files: `/dev/null`, `/dev/stdout`, `/dev/stderr`
pub fn is_safe_external_path(resolved: &Path) -> bool {
    // Device files — not real filesystem writes
    if resolved.starts_with("/dev/") {
        return true;
    }

    // Canonical /tmp (covers /private/tmp on macOS via symlink)
    let canonical_tmp = PathBuf::from("/tmp")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    if resolved.starts_with(&canonical_tmp) || resolved.starts_with("/tmp") {
        return true;
    }

    // $TMPDIR (e.g. /var/folders/.../T/ on macOS)
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        let tmpdir_path = PathBuf::from(&tmpdir);
        let canonical_tmpdir = tmpdir_path
            .canonicalize()
            .unwrap_or_else(|_| tmpdir_path.clone());
        if resolved.starts_with(&canonical_tmpdir) || resolved.starts_with(&tmpdir_path) {
            return true;
        }
    }

    // macOS scratch root — env-independent fallback for when `$TMPDIR` is
    // unset or stripped (sub-process env). `$TMPDIR` on macOS is always a
    // child of `/var/folders/<X>/<Y>/T/`; matching the parent prefix here
    // means we still allow scratchpad writes when the env var is missing.
    // The `/private/var/folders/` form covers the canonicalized symlink
    // target that `Path::canonicalize` returns on macOS.
    if resolved.starts_with("/var/folders/") || resolved.starts_with("/private/var/folders/") {
        return true;
    }

    // Koda cache zone: `~/.cache/koda/` and `$XDG_CACHE_HOME/koda/`.
    // These are functionally scratch — koda's own caches, not user data.
    // Treating them as outside-project would force a confirm prompt every
    // time the model wrote to its own cache, which is absurd.
    if let Some(home) = std::env::var_os("HOME") {
        let default_cache = PathBuf::from(&home).join(".cache").join("koda");
        if resolved.starts_with(&default_cache) {
            return true;
        }
    }
    if let Some(xdg_cache) = std::env::var_os("XDG_CACHE_HOME") {
        let xdg_koda = PathBuf::from(&xdg_cache).join("koda");
        if resolved.starts_with(&xdg_koda) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #1232 §8b: scratch-zone allowlist ──────────────────────────

    /// `/var/folders/.../T/...` (macOS scratch root) must be allowed
    /// even when `$TMPDIR` is unset — sub-process envs sometimes
    /// strip it, which pre-PR caused a confirm prompt on every
    /// scratchpad write. The env-independent prefix match is the
    /// safety belt.
    #[test]
    fn test_var_folders_allowed_without_tmpdir_env() {
        // Save + clear $TMPDIR for this assertion.
        // SAFETY: tests in the same process can race on env vars; this
        // module's tests are CPU-bound and tokio-free, but be cautious.
        let saved = std::env::var("TMPDIR").ok();
        // SAFETY: tests run single-threaded within a module by default;
        // mutating env here is the simplest way to assert the
        // env-independent fallback.
        unsafe {
            std::env::remove_var("TMPDIR");
        }

        let p = PathBuf::from("/var/folders/xx/yy/T/scratch.txt");
        let safe = is_safe_external_path(&p);

        // Restore env BEFORE asserting so a panic doesn't leak state.
        unsafe {
            if let Some(v) = saved {
                std::env::set_var("TMPDIR", v);
            }
        }

        assert!(
            safe,
            "/var/folders/* must be allowed without $TMPDIR — sub-process \
             env strip was the actual #1232 §8b repro"
        );
    }

    /// Canonicalized macOS form (`/private/var/folders/...`) must
    /// also be allowed — `Path::canonicalize` returns this on macOS
    /// because `/var` symlinks to `/private/var`. Without this the
    /// `is_outside_project` caller (which canonicalizes before
    /// matching) would skip our `/var/folders/` arm.
    #[test]
    fn test_private_var_folders_canonical_form_allowed() {
        let p = PathBuf::from("/private/var/folders/xx/yy/T/scratch.txt");
        assert!(
            is_safe_external_path(&p),
            "canonicalized macOS scratch path must be allowed"
        );
    }

    /// `~/.cache/koda/` is functionally koda's own cache — not user
    /// data. Treating it as outside-project would force confirm
    /// prompts every time the model wrote to its own cache.
    #[test]
    fn test_home_cache_koda_allowed() {
        let saved = std::env::var("HOME").ok();
        // SAFETY: see test_var_folders_allowed_without_tmpdir_env.
        unsafe {
            std::env::set_var("HOME", "/home/test-user");
        }

        let p = PathBuf::from("/home/test-user/.cache/koda/sessions/abc.json");
        let safe = is_safe_external_path(&p);

        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(safe, "~/.cache/koda/ must be allowed (#1232 §8b)");
    }

    /// XDG fallback: `$XDG_CACHE_HOME/koda/` (when set) takes the
    /// place of `~/.cache/koda/` on XDG-compliant systems. Both
    /// must work — no preference, just "if either matches, allow."
    #[test]
    fn test_xdg_cache_home_koda_allowed() {
        let saved = std::env::var("XDG_CACHE_HOME").ok();
        // SAFETY: see test_var_folders_allowed_without_tmpdir_env.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "/custom/cache");
        }

        let p = PathBuf::from("/custom/cache/koda/whatever.json");
        let safe = is_safe_external_path(&p);

        unsafe {
            match saved {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
        }

        assert!(safe, "$XDG_CACHE_HOME/koda/ must be allowed (#1232 §8b)");
    }

    /// Negative case: a sibling cache dir under `~/.cache/` that's
    /// NOT koda's must still be gated. We only allowlist koda's
    /// own scratch — not arbitrary `~/.cache/whatever/`.
    #[test]
    fn test_other_cache_dir_still_gated() {
        let saved = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", "/home/test-user");
        }

        let p = PathBuf::from("/home/test-user/.cache/some-other-tool/data.bin");
        let safe = is_safe_external_path(&p);

        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }

        assert!(
            !safe,
            "~/.cache/<not-koda>/ must NOT be allowlisted — we only \
             trust koda's own cache zone, not the entire ~/.cache tree"
        );
    }

    /// Negative case: a file directly under /var/ but NOT in /var/folders/
    /// is still gated. Catches a too-broad regex regression.
    #[test]
    fn test_var_log_still_gated() {
        let p = PathBuf::from("/var/log/system.log");
        assert!(
            !is_safe_external_path(&p),
            "/var/log/* must remain gated — only /var/folders/* is scratch"
        );
    }
}
