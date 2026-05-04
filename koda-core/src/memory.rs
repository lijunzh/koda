//! Semantic memory: project context injected into the system prompt.
//!
//! Memory is stored as human-readable Markdown, loaded from two tiers:
//!
//! **Global** (`~/.config/koda/memory.md`):
//!   User-wide preferences and conventions that apply to all projects.
//!
//! **Project-local** (first match wins):
//!   1. `MEMORY.md`  — Koda native
//!   2. `CLAUDE.md`  — Claude Code compatibility
//!   3. `AGENTS.md`  — Code Puppy compatibility
//!
//! Both tiers are concatenated and injected into the system prompt.
//! When Koda writes (auto-memory), it always writes to `MEMORY.md`.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Project-local memory files, checked in priority order.
const PROJECT_MEMORY_FILES: &[&str] = &["MEMORY.md", "CLAUDE.md", "AGENTS.md"];

/// Global memory filename inside `~/.config/koda/`.
const GLOBAL_MEMORY_FILE: &str = "memory.md";

/// Koda's native project memory filename (used for writes).
const KODA_MEMORY_FILE: &str = "MEMORY.md";

/// Per-file memoization for [`load`] (#1232 §3b).
///
/// Pre-fix, every sub-agent dispatch called `memory::load(project_root)`
/// and re-read `CLAUDE.md` / `MEMORY.md` from disk. The bug-review
/// session that opened #1232 logged `Loaded project memory from
/// CLAUDE.md (24200 bytes)` four times in a 200ms window for a
/// parallel-batch fan-out — 4× the disk I/O AND 4× the duplicate
/// log spam.
///
/// This cache memoizes by **resolved file path** with **(mtime, len)
/// invalidation**: if the file is unchanged since the last read we
/// return the cached content; if it was edited (e.g. via the
/// `MemoryWrite` tool's `append` path below) the next `load` call
/// detects the new mtime, re-reads, and updates the cache. No
/// explicit invalidation hook is needed because the filesystem
/// itself is the source of truth.
///
/// Why `(mtime, len)` not just `mtime`: HFS+ on macOS has
/// 1-second mtime resolution, so a same-second edit that doesn't
/// change file size COULD slip past an mtime-only check. Adding
/// `len` makes that pathological case still safe for the common
/// "the model added a line" pattern.
///
/// `len` is u64 so it matches `Metadata::len()` directly; no width
/// math, no surprises on >4GB memory files (humans should not have
/// >4GB memory files, but the type is free).
struct CachedEntry {
    mtime: SystemTime,
    len: u64,
    content: String,
}

/// Process-wide cache. `OnceLock<Mutex<HashMap>>` keeps init
/// trivially thread-safe without an external dependency. The cache
/// is unbounded but in practice only holds entries for paths the
/// current process has actually touched (typically 1-3 paths per
/// koda session: maybe one project memory + the global file).
static MEMORY_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedEntry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<PathBuf, CachedEntry>> {
    MEMORY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Read `path` through the memoization cache (#1232 §3b).
///
/// Returns `(content, was_cache_hit)`. Caller decides what (if anything)
/// to log based on the second tuple element — the cache logs at
/// `debug!` only, leaving the existing `info!` "Loaded ..." line for
/// genuine refreshes (cache miss or mtime-changed) so noisy parallel
/// fan-out no longer spams the log.
fn read_through_cache(path: &Path) -> Result<(String, bool)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?;
    let len = meta.len();

    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    // Fast path: cache hit.
    {
        let map = cache().lock().expect("memory cache mutex poisoned");
        if let Some(entry) = map.get(&canonical)
            && entry.mtime == mtime
            && entry.len == len
        {
            tracing::debug!(
                path = %canonical.display(),
                bytes = entry.content.len(),
                "memory cache hit"
            );
            return Ok((entry.content.clone(), true));
        }
    }

    // Slow path: read + insert. Done outside the lock so concurrent
    // readers of OTHER paths aren't blocked on disk I/O.
    let content = std::fs::read_to_string(path)?;
    {
        let mut map = cache().lock().expect("memory cache mutex poisoned");
        map.insert(
            canonical,
            CachedEntry {
                mtime,
                len,
                content: content.clone(),
            },
        );
    }
    Ok((content, false))
}

/// Clear the in-process memory cache. **Test-only.**
///
/// Required by tests that mutate memory files directly (bypassing
/// `append` / `append_global`) or that need a guaranteed cold-cache
/// starting state. Production code never needs this — mtime
/// invalidation handles every legitimate edit path.
#[cfg(test)]
pub(crate) fn clear_cache_for_tests() {
    if let Some(m) = MEMORY_CACHE.get() {
        m.lock().expect("memory cache mutex poisoned").clear();
    }
}

/// Load memory from both global and project-local sources.
///
/// Returns the combined content (global first, then project-local).
/// Returns an empty string if no memory files exist.
///
/// **#1232 §3b**: this function is memoized per resolved file path
/// with `(mtime, len)` invalidation. Calling it N times in a row
/// without touching the underlying files (e.g. parallel sub-agent
/// fan-out) reads disk exactly once and emits the `info!` log line
/// exactly once. Edits via `append` / `append_global` (or any
/// out-of-process `MemoryWrite`) update mtime, so the next call
/// re-reads transparently.
pub fn load(project_root: &Path) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();

    // 1. Global memory (~/.config/koda/memory.md)
    if let Some((content, was_hit)) = load_global()? {
        if !was_hit {
            tracing::info!("Loaded global memory ({} bytes)", content.len());
        }
        parts.push(content);
    }

    // 2. Project-local memory (first match wins)
    if let Some((filename, content, was_hit)) = load_project(project_root)? {
        if !was_hit {
            tracing::info!(
                "Loaded project memory from {filename} ({} bytes)",
                content.len()
            );
        }
        parts.push(content);
    } else {
        // "no file" log stays at info; it's once-per-load and
        // genuinely informative for diagnosing missing-memory
        // confusion. Not a cache concern (nothing to cache).
        tracing::info!("No project memory file found");
    }

    Ok(parts.join("\n\n"))
}

/// Write an entry to the project's memory file.
///
/// If the entry starts with a `## Heading`, and a section with that
/// heading already exists in the file, the existing section is
/// **replaced** (updated in place). Otherwise the entry is appended.
///
/// Always targets the active memory file (or `MEMORY.md` if none exists).
pub fn append(project_root: &Path, entry: &str) -> Result<()> {
    let target_filename =
        active_project_file(project_root).unwrap_or_else(|| KODA_MEMORY_FILE.to_string());
    let path = project_root.join(&target_filename);
    write_or_replace_section(&path, entry)?;
    tracing::info!("Wrote to {target_filename}: {entry}");
    Ok(())
}

/// Return which project memory file is active (for display purposes).
pub fn active_project_file(project_root: &Path) -> Option<String> {
    for filename in PROJECT_MEMORY_FILES {
        if project_root.join(filename).exists() {
            return Some(filename.to_string());
        }
    }
    None
}

/// Write an entry to the global memory file (~/.config/koda/memory.md).
///
/// If the entry starts with a `## Heading` that already exists, the
/// section is replaced. Otherwise the entry is appended.
pub fn append_global(entry: &str) -> Result<()> {
    let path = global_memory_path()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory for global memory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_or_replace_section(&path, entry)?;
    tracing::info!("Wrote to global memory: {entry}");
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Write an entry to a memory file, merging by `## Heading` if possible.
///
/// If `entry` starts with `## <heading>`, we look for an existing section
/// with the same heading. If found, the old section (heading through to
/// the next `##` heading or EOF) is replaced with `entry`. If not found
/// (or `entry` has no heading), the entry is appended.
fn write_or_replace_section(path: &Path, entry: &str) -> Result<()> {
    let heading = extract_heading(entry);
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let new_content = match heading {
        Some(ref h) if section_exists(&existing, h) => replace_section(&existing, h, entry),
        _ => {
            // No heading or heading not found → append
            let mut buf = existing;
            if !buf.is_empty() && !buf.ends_with('\n') {
                buf.push('\n');
            }
            buf.push_str(&format!("\n- {entry}"));
            buf.push('\n');
            buf
        }
    };

    std::fs::write(path, new_content)?;
    Ok(())
}

/// Extract a `## Heading` from the first line of an entry.
fn extract_heading(entry: &str) -> Option<String> {
    let first_line = entry.lines().next()?.trim();
    if first_line.starts_with("## ") {
        Some(first_line.to_string())
    } else {
        None
    }
}

/// Check if a `## Heading` section already exists in the content.
fn section_exists(content: &str, heading: &str) -> bool {
    content.lines().any(|line| line.trim() == heading)
}

/// Replace a `## Heading` section with new content.
///
/// The section spans from the heading line to (but not including)
/// the next `## ` heading or EOF.
fn replace_section(content: &str, heading: &str, replacement: &str) -> String {
    let mut result = String::new();
    let mut in_target_section = false;
    let mut replaced = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed == heading && !replaced {
            // Start of the section we want to replace
            in_target_section = true;
            // Emit the replacement content
            result.push_str(replacement);
            if !replacement.ends_with('\n') {
                result.push('\n');
            }
            replaced = true;
            continue;
        }

        if in_target_section {
            // Check if we've hit the next section heading
            if trimmed.starts_with("## ") {
                in_target_section = false;
                result.push_str(line);
                result.push('\n');
            }
            // else: skip old section content
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
}

/// Load global memory from `~/.config/koda/memory.md`.
///
/// Returns `(content, was_cache_hit)` so the caller can suppress
/// duplicate log lines on cache-hit (#1232 §3b).
fn load_global() -> Result<Option<(String, bool)>> {
    let path = global_memory_path();
    match path {
        Some(p) if p.exists() => {
            let (content, was_hit) = read_through_cache(&p)?;
            if content.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some((content, was_hit)))
            }
        }
        _ => Ok(None),
    }
}

/// Load project-local memory (first matching file wins).
///
/// Returns `(filename, content, was_cache_hit)` so the caller can
/// suppress duplicate log lines on cache-hit (#1232 §3b).
fn load_project(project_root: &Path) -> Result<Option<(String, String, bool)>> {
    for filename in PROJECT_MEMORY_FILES {
        let path = project_root.join(filename);
        if path.exists() {
            let (content, was_hit) = read_through_cache(&path)?;
            if !content.trim().is_empty() {
                return Ok(Some((filename.to_string(), content, was_hit)));
            }
        }
    }
    Ok(None)
}

/// Path to the global memory file.
fn global_memory_path() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("koda")
            .join(GLOBAL_MEMORY_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_missing_memory_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_load_memory_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "# Project notes\n- Uses Rust").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("Uses Rust"));
    }

    #[test]
    fn test_load_claude_md_compat() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude rules\n- Be concise").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("Be concise"));
    }

    #[test]
    fn test_load_agents_md_compat() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Agent rules\n- DRY").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("DRY"));
    }

    #[test]
    fn test_memory_md_takes_priority_over_claude_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "koda-memory").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-rules").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("koda-memory"));
        assert!(!content.contains("claude-rules"));
    }

    #[test]
    fn test_claude_md_takes_priority_over_agents_md() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-rules").unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "puppy-rules").unwrap();
        let content = load(tmp.path()).unwrap();
        assert!(content.contains("claude-rules"));
        assert!(!content.contains("puppy-rules"));
    }

    #[test]
    fn test_append_creates_and_appends() {
        let tmp = TempDir::new().unwrap();
        append(tmp.path(), "first entry").unwrap();
        append(tmp.path(), "second entry").unwrap();

        let content = load(tmp.path()).unwrap();
        assert!(content.contains("first entry"));
        assert!(content.contains("second entry"));
    }

    #[test]
    fn test_append_writes_to_active_file() {
        let tmp = TempDir::new().unwrap();
        // If CLAUDE.md exists, append writes directly to CLAUDE.md
        std::fs::write(tmp.path().join("CLAUDE.md"), "existing claude rules").unwrap();
        append(tmp.path(), "new koda insight").unwrap();

        // It should NOT create MEMORY.md
        assert!(!tmp.path().join("MEMORY.md").exists());

        // It SHOULD append to CLAUDE.md
        let memory = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(memory.contains("new koda insight"));
    }

    #[test]
    fn test_active_project_file() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(active_project_file(tmp.path()), None);

        std::fs::write(tmp.path().join("AGENTS.md"), "rules").unwrap();
        assert_eq!(
            active_project_file(tmp.path()),
            Some("AGENTS.md".to_string())
        );

        std::fs::write(tmp.path().join("MEMORY.md"), "memory").unwrap();
        assert_eq!(
            active_project_file(tmp.path()),
            Some("MEMORY.md".to_string())
        );
    }

    // ── Section merge tests (#519) ──

    #[test]
    fn test_extract_heading() {
        assert_eq!(
            extract_heading("## Workflow Preferences\n- item"),
            Some("## Workflow Preferences".to_string())
        );
        assert_eq!(extract_heading("just a plain note"), None);
        assert_eq!(extract_heading("# Top level heading"), None); // only ## is matched
        assert_eq!(extract_heading(""), None);
    }

    #[test]
    fn test_section_exists() {
        let content = "# Title\n## Workflow Preferences\n- item1\n## Other\n- item2";
        assert!(section_exists(content, "## Workflow Preferences"));
        assert!(section_exists(content, "## Other"));
        assert!(!section_exists(content, "## Missing"));
    }

    #[test]
    fn test_replace_section() {
        let content = "# Title\n## Workflow Preferences\n- old item1\n- old item2\n## Other Section\n- keep this\n";
        let replacement = "## Workflow Preferences\n- new item1\n- new item2\n- new item3";
        let result = replace_section(content, "## Workflow Preferences", replacement);
        assert!(result.contains("- new item1"), "Should contain new content");
        assert!(result.contains("- new item3"), "Should contain new content");
        assert!(
            !result.contains("- old item1"),
            "Should not contain old content"
        );
        assert!(
            result.contains("## Other Section"),
            "Should preserve other sections"
        );
        assert!(
            result.contains("- keep this"),
            "Should preserve other section content"
        );
    }

    #[test]
    fn test_replace_section_at_end() {
        let content = "## First\n- a\n## Second\n- old\n";
        let replacement = "## Second\n- new";
        let result = replace_section(content, "## Second", replacement);
        assert!(result.contains("## First"), "Should preserve first section");
        assert!(
            result.contains("- a"),
            "Should preserve first section content"
        );
        assert!(result.contains("- new"), "Should contain replacement");
        assert!(!result.contains("- old"), "Should not contain old content");
    }

    #[test]
    fn test_append_merges_existing_section() {
        let tmp = TempDir::new().unwrap();
        let existing = "## Workflow Preferences\n- old item\n";
        std::fs::write(tmp.path().join("MEMORY.md"), existing).unwrap();

        append(
            tmp.path(),
            "## Workflow Preferences\n- updated item\n- new item",
        )
        .unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(
            content.contains("- updated item"),
            "Should contain new content"
        );
        assert!(content.contains("- new item"), "Should contain new content");
        assert!(
            !content.contains("- old item"),
            "Should not contain old content"
        );
        // Should only have one copy of the heading
        assert_eq!(
            content.matches("## Workflow Preferences").count(),
            1,
            "Should have exactly one copy of the heading"
        );
    }

    #[test]
    fn test_append_new_section_still_appends() {
        let tmp = TempDir::new().unwrap();
        let existing = "## Existing Section\n- item\n";
        std::fs::write(tmp.path().join("MEMORY.md"), existing).unwrap();

        append(tmp.path(), "## New Section\n- new item").unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(content.contains("## Existing Section"));
        assert!(content.contains("## New Section"));
        assert!(content.contains("- new item"));
    }

    #[test]
    fn test_append_plain_entry_still_appends() {
        let tmp = TempDir::new().unwrap();
        append(tmp.path(), "just a plain note").unwrap();
        append(tmp.path(), "another plain note").unwrap();

        let content = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(content.contains("just a plain note"));
        assert!(content.contains("another plain note"));
    }

    // ── Memoization tests (#1232 §3b) ──────────────────────────────────
    //
    // The cache is process-wide. Tests below MUST be isolated by
    // using their own TempDir (different canonical paths → different
    // cache keys) and call `clear_cache_for_tests()` defensively in
    // case a future test mutates a shared path. The TempDir-per-test
    // pattern means concurrent test runs don't fight over keys.

    /// Cache hit: same project_root, no file mutations → second
    /// `load()` reads zero bytes from disk and returns identical content.
    ///
    /// We can't directly observe "no disk I/O happened" without
    /// instrumentation, so we use the inotify-style proxy: read once,
    /// snapshot the mtime, do a flurry of loads, confirm the cached
    /// content matches AND the file was never re-stat'd into a fresh
    /// entry (we'd see a different mtime if it were).
    #[test]
    fn test_cache_hit_skips_disk_reread() {
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "# pinned content\n- entry one").unwrap();

        let first = load(tmp.path()).unwrap();
        assert!(first.contains("entry one"));

        // Hammer the cache. Pre-fix this would have hit disk N times.
        // The output must stay byte-identical to the first read because
        // nothing on disk changed and the cache must serve every call.
        for _ in 0..10 {
            let again = load(tmp.path()).unwrap();
            assert_eq!(again, first, "cache must serve identical bytes");
        }

        // Snapshot the cache entry directly to confirm we recorded
        // exactly one entry for this canonical path (i.e. didn't
        // re-insert on every call).
        let canonical = path.canonicalize().unwrap();
        let map = cache().lock().unwrap();
        let entry = map.get(&canonical).expect("path must be cached");
        assert_eq!(entry.content, "# pinned content\n- entry one");
        assert_eq!(entry.len, std::fs::metadata(&path).unwrap().len());
    }

    /// Cache invalidation via `append`: writing through the public
    /// `append` API touches mtime, so the next `load` must detect
    /// the change and refresh.
    #[test]
    fn test_cache_invalidates_on_append() {
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "## Initial\n- one\n").unwrap();

        let before = load(tmp.path()).unwrap();
        assert!(before.contains("- one"));
        assert!(!before.contains("- two"));

        // Sleep just enough to guarantee a mtime tick on filesystems
        // with 1-second resolution (HFS+). Without this, an immediate
        // append could land in the same second AND happen to keep the
        // same len (impossible here — we're growing — but the sleep
        // is the principled defense).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        append(tmp.path(), "- two").unwrap();

        let after = load(tmp.path()).unwrap();
        assert!(
            after.contains("- two"),
            "post-append load must surface the new entry; got: {after:?}"
        );
    }

    /// Cache invalidation via direct disk mutation (out-of-process
    /// edit, or `MemoryWrite` that grows the file). This pins the
    /// `len` half of the `(mtime, len)` invariant: if mtime were the
    /// only key and the filesystem had coarse resolution, a same-
    /// second edit could slip through. Growing the file changes len
    /// even if mtime didn't, so the cache MUST refresh.
    #[test]
    fn test_cache_invalidates_on_size_change() {
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "short").unwrap();

        let before = load(tmp.path()).unwrap();
        assert!(before.contains("short"));

        // Overwrite with a larger payload. Even on coarse-mtime
        // filesystems the len delta forces a cache refresh.
        std::fs::write(&path, "this is much longer content than before").unwrap();
        let after = load(tmp.path()).unwrap();
        assert!(
            after.contains("much longer"),
            "len-delta must trigger a refresh; got: {after:?}"
        );
    }

    /// Different project roots get separate cache entries — the cache
    /// is keyed on the canonical file path, not the project root.
    /// Two TempDirs each with their own CLAUDE.md must not
    /// cross-contaminate.
    #[test]
    fn test_cache_isolates_by_path() {
        clear_cache_for_tests();
        let proj_a = TempDir::new().unwrap();
        let proj_b = TempDir::new().unwrap();
        std::fs::write(proj_a.path().join("CLAUDE.md"), "alpha-content").unwrap();
        std::fs::write(proj_b.path().join("CLAUDE.md"), "beta-content").unwrap();

        let a = load(proj_a.path()).unwrap();
        let b = load(proj_b.path()).unwrap();

        assert!(a.contains("alpha-content"));
        assert!(!a.contains("beta-content"));
        assert!(b.contains("beta-content"));
        assert!(!b.contains("alpha-content"));

        // Both paths are now cached independently. Toggle-load to
        // confirm neither cache entry was clobbered by the other.
        assert_eq!(load(proj_a.path()).unwrap(), a);
        assert_eq!(load(proj_b.path()).unwrap(), b);
    }

    /// Concurrent loads on the same project: the cache must serve
    /// all of them with at most ONE disk read. Models the bug-review
    /// scenario directly — parallel sub-agent fan-out hammering
    /// `memory::load` from N tokio tasks.
    #[test]
    fn test_concurrent_loads_share_cache() {
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "shared-by-many").unwrap();
        let project_root = tmp.path().to_path_buf();

        // 8 threads × 10 calls each = 80 loads. With the cache, only
        // the first thread's first call should actually hit disk;
        // the other 79 must return cached content.
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let pr = project_root.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        let c = load(&pr).unwrap();
                        assert!(c.contains("shared-by-many"));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }

        // Exactly one cache entry exists for this canonical path.
        let canonical = path.canonicalize().unwrap();
        let map = cache().lock().unwrap();
        assert!(
            map.contains_key(&canonical),
            "cache must contain entry for {canonical:?}"
        );
    }
}
