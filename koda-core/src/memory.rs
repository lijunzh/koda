//! Memory: the instructions people write, and the notes Koda keeps.
//!
//! Both are Markdown, sent at the start of the first user message.
//!
//! **Instruction files** (written by people, read in this order, Codex's
//! rules with Claude Code's file as the fallback):
//!   1. Global: `<config>/AGENTS.override.md`, else `<config>/AGENTS.md`.
//!   2. Project: every folder from the git root down to the project root,
//!      root first. In each folder the first of `AGENTS.override.md`,
//!      `AGENTS.md` and `CLAUDE.md`. So a folder with both `AGENTS.md` and
//!      `CLAUDE.md` gives one file, which the eval harness relies on: it
//!      writes the same text under both names.
//!
//! `<config>` is [`crate::db::config_dir`], `~/.config/koda` unless
//! `XDG_CONFIG_HOME` moves it. The older names, a project `MEMORY.md` and
//! a global `memory.md`, are still read where nothing newer exists, with a
//! warning to rename them.
//!
//! **Koda's notes** (`MemoryWrite`, `/memory add`): never in the project,
//! where they'd land in a file the whole team reads. They go to
//! `<config>/memories/<project>/MEMORY.md`, or `<config>/memories/MEMORY.md`
//! for the global scope, the layout Codex's memory pipeline uses
//! (`~/.codex/memories`). `<project>` is the project's path with each
//! separator turned into `-`, as Claude Code names its project folders.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Instruction files in a folder, in priority order: the first one found
/// is the folder's.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"];

/// Global instruction files in the config directory, in priority order.
const GLOBAL_INSTRUCTION_FILES: &[&str] = &["AGENTS.override.md", "AGENTS.md"];

/// The project instruction file before `AGENTS.md`; read with a warning.
const LEGACY_PROJECT_FILE: &str = "MEMORY.md";

/// The global instruction file before `AGENTS.md`; read with a warning.
const LEGACY_GLOBAL_FILE: &str = "memory.md";

/// Koda's notes folder inside the config directory.
const NOTES_DIR: &str = "memories";

/// The notes file in each notes folder.
const NOTES_FILE: &str = "MEMORY.md";

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

/// Legacy files already warned about, so a warning is logged once per
/// process, not on every agent and sub-agent start.
static WARNED_LEGACY: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

/// Log `message` the first time `path` is seen.
fn warn_once(path: &Path, message: &str) {
    let mut seen = WARNED_LEGACY
        .get_or_init(|| Mutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if seen.insert(path.to_path_buf()) {
        tracing::warn!("{}: {message}", path.display());
    }
}

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
    //
    // We use `unwrap_or_else(|p| p.into_inner())` instead of `expect()`
    // so a panic in one test that happened to be holding this lock
    // doesn't cascade-fail every other memory test in the same process
    // with `PoisonError`. See #1333. In production this branch is
    // unreachable (no test-only code calls `panic!()` while holding the
    // cache mutex) so the change is a defensive no-op outside tests.
    {
        let map = cache().lock().unwrap_or_else(|p| p.into_inner());
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
        let mut map = cache().lock().unwrap_or_else(|p| p.into_inner());
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
        m.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

/// Test-only serialization mutex for memory tests.
///
/// Why this exists (#1333):
///
/// `memory::tests::*` and `tools::memory::tests::*` share a single
/// process-wide [`MEMORY_CACHE`]. They mutate it via
/// [`clear_cache_for_tests`] and snapshot it via direct
/// `cache().lock()` calls. Under `cargo test` (parallel by default)
/// one test could clear the cache between another test's
/// `load()`+`cache().lock().get(...).expect(...)` sequence, panicking
/// **while holding the lock** and poisoning it for every subsequent
/// test in the process.
///
/// Acquire this lock at the top of every memory test that touches
/// the cache:
///
/// ```ignore
/// let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
/// clear_cache_for_tests();
/// // ... test body ...
/// ```
///
/// The `unwrap_or_else(|p| p.into_inner())` makes acquisition
/// poison-resilient — if a prior test panicked while holding it, the
/// next test still proceeds (so genuine bugs surface as their own
/// assertion failures, not as N cascading `PoisonError`s).
///
/// Mirrors `koda_test_utils::ENV_MUTEX` (the canonical pattern for
/// env-var test isolation).
#[cfg(test)]
pub(crate) static MEMORY_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Load the instruction files and Koda's notes for `project_root`.
///
/// Returns them joined with blank lines: global instructions, then the
/// project's from the git root down, then Koda's global and project notes.
/// Returns an empty string if there are none.
///
/// **#1232 §3b**: memoized per resolved file path with `(mtime, len)`
/// invalidation, so parallel sub-agent fan-out reads each file once.
pub fn load(project_root: &Path) -> Result<String> {
    load_in(project_root, crate::db::config_dir().ok().as_deref())
}

pub(crate) fn load_in(project_root: &Path, config: Option<&Path>) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut push = |path: &Path, what: &str| -> Result<()> {
        let (content, was_hit) = read_through_cache(path)?;
        if content.trim().is_empty() {
            return Ok(());
        }
        if !was_hit {
            tracing::info!(
                "Loaded {what} from {} ({} bytes)",
                path.display(),
                content.len()
            );
        }
        parts.push(content);
        Ok(())
    };

    if let Some(path) = config.and_then(global_instruction_file) {
        push(&path, "global instructions")?;
    }
    let project_files = project_instruction_files(project_root);
    if project_files.is_empty() {
        // Debug, not info: memory reloads every turn.
        tracing::debug!("No project instruction file found");
    }
    for path in &project_files {
        push(path, "project instructions")?;
    }
    if let Some(config) = config {
        for path in [
            global_notes_file(config),
            project_notes_file(config, project_root),
        ] {
            if path.is_file() {
                push(&path, "Koda's notes")?;
            }
        }
    }
    Ok(parts.join("\n\n"))
}

/// Save `entry` to Koda's notes for this project, outside the project.
///
/// If the entry starts with a `## Heading`, and a section with that
/// heading already exists, that section is **replaced**. Otherwise the
/// entry is added.
pub fn append(project_root: &Path, entry: &str) -> Result<()> {
    let path = project_notes_path(project_root)?;
    write_note(&path, entry)
}

/// Save `entry` to Koda's global notes (`<config>/memories/MEMORY.md`).
pub fn append_global(entry: &str) -> Result<()> {
    let path = global_notes_path()?;
    write_note(&path, entry)
}

/// The instruction file Koda reads at the project root, for display.
pub fn active_project_file(project_root: &Path) -> Option<String> {
    folder_instruction_file(project_root)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// Where Koda's notes for this project are kept.
pub fn project_notes_path(project_root: &Path) -> Result<PathBuf> {
    Ok(project_notes_file(&crate::db::config_dir()?, project_root))
}

/// Where Koda's global notes are kept.
pub fn global_notes_path() -> Result<PathBuf> {
    Ok(global_notes_file(&crate::db::config_dir()?))
}

/// [`append`] with an explicit config directory.
pub(crate) fn append_in(config: &Path, project_root: &Path, entry: &str) -> Result<()> {
    write_note(&project_notes_file(config, project_root), entry)
}

/// [`append_global`] with an explicit config directory.
pub(crate) fn append_global_in(config: &Path, entry: &str) -> Result<()> {
    write_note(&global_notes_file(config), entry)
}

/// [`project_notes_path`] with an explicit config directory.
pub(crate) fn project_notes_path_in(config: &Path, project_root: &Path) -> PathBuf {
    project_notes_file(config, project_root)
}

fn write_note(path: &Path, entry: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_or_replace_section(path, entry)?;
    tracing::info!("Wrote to {}: {entry}", path.display());
    Ok(())
}

fn global_notes_file(config: &Path) -> PathBuf {
    config.join(NOTES_DIR).join(NOTES_FILE)
}

fn project_notes_file(config: &Path, project_root: &Path) -> PathBuf {
    config
        .join(NOTES_DIR)
        .join(project_key(project_root))
        .join(NOTES_FILE)
}

/// The project's folder name under `memories/`: its absolute path with each
/// separator (and a Windows drive's `:`) turned into `-`, as Claude Code
/// names its project folders (`/Users/me/repo/koda` → `-Users-me-repo-koda`).
fn project_key(project_root: &Path) -> String {
    let path = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    path.to_string_lossy()
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// The global instruction file, if any: `AGENTS.override.md`, then
/// `AGENTS.md`, then the legacy `memory.md` with a warning.
fn global_instruction_file(config: &Path) -> Option<PathBuf> {
    if let Some(path) = first_existing(config, GLOBAL_INSTRUCTION_FILES) {
        return Some(path);
    }
    let legacy = config.join(LEGACY_GLOBAL_FILE);
    legacy.is_file().then(|| {
        warn_once(
            &legacy,
            "the global memory.md is deprecated: rename it to AGENTS.md if it holds \
             instructions, or move it to memories/MEMORY.md if it holds Koda's notes",
        );
        legacy
    })
}

/// One instruction file per folder from the git root down to
/// `project_root`, root first. Without a git root, `project_root` alone.
/// Codex reads the same span and never walks past the git root.
fn project_instruction_files(project_root: &Path) -> Vec<PathBuf> {
    let mut folders = vec![project_root.to_path_buf()];
    let mut cursor = project_root;
    while !cursor.join(".git").exists() {
        match cursor.parent() {
            Some(parent) => {
                folders.push(parent.to_path_buf());
                cursor = parent;
            }
            // No git root above: only the project root counts.
            None => {
                folders.truncate(1);
                break;
            }
        }
    }
    folders.reverse();
    folders
        .iter()
        .filter_map(|folder| folder_instruction_file(folder))
        .collect()
}

/// A folder's instruction file: the first of [`INSTRUCTION_FILES`], else
/// the legacy `MEMORY.md` with a warning.
fn folder_instruction_file(folder: &Path) -> Option<PathBuf> {
    if let Some(path) = first_existing(folder, INSTRUCTION_FILES) {
        return Some(path);
    }
    let legacy = folder.join(LEGACY_PROJECT_FILE);
    legacy.is_file().then(|| {
        warn_once(
            &legacy,
            "a project MEMORY.md is deprecated: rename it to AGENTS.md if it holds \
             instructions, or move it out of the project (see /memory) if it holds \
             Koda's notes",
        );
        legacy
    })
}

fn first_existing(folder: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| folder.join(name))
        .find(|path| path.is_file())
}

// ── Internal helpers ──────────────────────────────────────────────────────

/// Write an entry to a memory file, merging by `## Heading` if possible.
///
/// If `entry` starts with `## <heading>`, we look for an existing section
/// with the same heading. If found, the old section (heading through to
/// the next `##` heading or EOF) is replaced with `entry`. If not found, a
/// new section goes at the end. An entry with no heading goes above the first
/// section, because replacing a section takes everything up to the next one.
fn write_or_replace_section(path: &Path, entry: &str) -> Result<()> {
    let heading = extract_heading(entry);
    let existing = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };

    let new_content = match heading {
        Some(ref h) if section_exists(&existing, h) => replace_section(&existing, h, entry),
        Some(_) => append_block(existing, entry),
        None => match first_section_start(&existing) {
            Some(at) => format!("{}- {entry}\n\n{}", &existing[..at], &existing[at..]),
            None => append_block(existing, &format!("- {entry}")),
        },
    };

    std::fs::write(path, new_content)?;
    Ok(())
}

/// Add `block` to the end of `content`, after a blank line.
fn append_block(mut content: String, block: &str) -> String {
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("\n{block}\n"));
    content
}

/// Byte offset of the first `## ` heading line.
fn first_section_start(content: &str) -> Option<usize> {
    content
        .match_indices("## ")
        .map(|(at, _)| at)
        .find(|&at| at == 0 || content.as_bytes()[at - 1] == b'\n')
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A project in its own git repository, with a separate config dir.
    fn project() -> (TempDir, TempDir) {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        (root, TempDir::new().unwrap())
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn test_load_missing_memory_returns_empty() {
        let (root, config) = project();
        assert!(
            load_in(root.path(), Some(config.path()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn agents_md_is_read_first_and_claude_md_is_the_fallback() {
        let (root, config) = project();
        write(&root.path().join("CLAUDE.md"), "claude-rules");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert!(content.contains("claude-rules"));

        write(&root.path().join("AGENTS.md"), "agents-rules");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert!(content.contains("agents-rules"));
        // One file per folder: the eval harness writes the same text under
        // both names, and the model must get it once.
        assert!(!content.contains("claude-rules"), "{content}");

        write(&root.path().join("AGENTS.override.md"), "override-rules");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert_eq!(content, "override-rules");
    }

    #[test]
    fn memory_md_is_read_only_when_nothing_newer_exists() {
        let (root, config) = project();
        write(&root.path().join("MEMORY.md"), "legacy-memory");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert!(content.contains("legacy-memory"));

        write(&root.path().join("CLAUDE.md"), "claude-rules");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert!(content.contains("claude-rules"));
        assert!(!content.contains("legacy-memory"), "{content}");
    }

    #[test]
    fn folders_from_the_git_root_down_are_joined_root_first() {
        let (root, config) = project();
        let sub = root.path().join("crates/app");
        write(&root.path().join("AGENTS.md"), "root-rules");
        write(&root.path().join("crates/CLAUDE.md"), "crates-rules");
        write(&sub.join("AGENTS.md"), "app-rules");
        let content = load_in(&sub, Some(config.path())).unwrap();
        assert_eq!(content, "root-rules\n\ncrates-rules\n\napp-rules");
    }

    #[test]
    fn without_a_git_root_only_the_project_folder_counts() {
        let parent = TempDir::new().unwrap();
        let config = TempDir::new().unwrap();
        let sub = parent.path().join("sub");
        write(&parent.path().join("AGENTS.md"), "parent-rules");
        write(&sub.join("AGENTS.md"), "sub-rules");
        let content = load_in(&sub, Some(config.path())).unwrap();
        assert_eq!(content, "sub-rules");
    }

    #[test]
    fn global_agents_md_comes_first_and_memory_md_is_its_fallback() {
        let (root, config) = project();
        write(&root.path().join("AGENTS.md"), "project-rules");
        write(&config.path().join("memory.md"), "legacy-global");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert_eq!(content, "legacy-global\n\nproject-rules");

        write(&config.path().join("AGENTS.md"), "global-rules");
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert_eq!(content, "global-rules\n\nproject-rules");
    }

    #[test]
    fn notes_are_kept_outside_the_project_and_loaded_after_instructions() {
        let (root, config) = project();
        write(&root.path().join("CLAUDE.md"), "project-rules");
        append_in(config.path(), root.path(), "first entry").unwrap();
        append_in(config.path(), root.path(), "second entry").unwrap();

        // Nothing written into the project.
        let claude = std::fs::read_to_string(root.path().join("CLAUDE.md")).unwrap();
        assert_eq!(claude, "project-rules");
        assert!(!root.path().join("MEMORY.md").exists());

        let notes = project_notes_file(config.path(), root.path());
        assert!(
            notes.starts_with(config.path().join("memories")),
            "{notes:?}"
        );
        let content = load_in(root.path(), Some(config.path())).unwrap();
        assert!(content.starts_with("project-rules\n\n"), "{content}");
        assert!(content.contains("first entry") && content.contains("second entry"));
    }

    #[test]
    fn each_project_has_its_own_notes() {
        let (a, config) = project();
        let b = TempDir::new().unwrap();
        std::fs::create_dir(b.path().join(".git")).unwrap();
        append_in(config.path(), a.path(), "alpha note").unwrap();
        let in_b = load_in(b.path(), Some(config.path())).unwrap();
        assert!(!in_b.contains("alpha note"), "{in_b}");
        write(&global_notes_file(config.path()), "- shared note\n");
        let in_b = load_in(b.path(), Some(config.path())).unwrap();
        assert!(in_b.contains("shared note"), "{in_b}");
    }

    #[test]
    fn project_key_turns_separators_into_dashes() {
        let key = project_key(Path::new("/no/such/dir/koda"));
        assert_eq!(key, "-no-such-dir-koda");
    }

    #[test]
    fn test_active_project_file() {
        let (root, _config) = project();
        assert_eq!(active_project_file(root.path()), None);
        write(&root.path().join("CLAUDE.md"), "rules");
        assert_eq!(
            active_project_file(root.path()).as_deref(),
            Some("CLAUDE.md")
        );
        write(&root.path().join("AGENTS.md"), "rules");
        assert_eq!(
            active_project_file(root.path()).as_deref(),
            Some("AGENTS.md")
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
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        write(&notes, "## Workflow Preferences\n- old item\n");

        append_in(
            config.path(),
            tmp.path(),
            "## Workflow Preferences\n- updated item\n- new item",
        )
        .unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
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
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        write(&notes, "## Existing Section\n- item\n");

        append_in(config.path(), tmp.path(), "## New Section\n- new item").unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
        assert!(content.contains("## Existing Section"));
        assert!(content.contains("## New Section"));
        assert!(content.contains("- new item"));
    }

    #[test]
    fn test_append_replaces_a_section_it_wrote() {
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        append_in(config.path(), tmp.path(), "## Preferences\n- old").unwrap();
        append_in(config.path(), tmp.path(), "## Preferences\n- new").unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
        assert!(content.contains("- new"));
        assert!(!content.contains("- old"), "{content}");
        assert_eq!(content.matches("## Preferences").count(), 1, "{content}");
        assert!(
            content.lines().any(|line| line == "## Preferences"),
            "the heading must stay a heading: {content}"
        );
    }

    #[test]
    fn test_plain_note_survives_a_section_update() {
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        append_in(config.path(), tmp.path(), "## Preferences\n- old").unwrap();
        append_in(config.path(), tmp.path(), "keep this note").unwrap();
        append_in(config.path(), tmp.path(), "## Preferences\n- new").unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
        assert!(content.contains("- keep this note"), "{content}");
        assert!(content.contains("- new"));
        assert!(!content.contains("- old"), "{content}");
    }

    #[test]
    fn test_plain_entry_stays_after_a_preamble() {
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        write(
            &notes,
            "# Title\n\nIntro with a ## in it.\n\n## Section\n- item\n",
        );

        append_in(config.path(), tmp.path(), "a note").unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
        assert_eq!(
            content,
            "# Title\n\nIntro with a ## in it.\n\n- a note\n\n## Section\n- item\n"
        );
    }

    #[test]
    fn test_append_plain_entry_still_appends() {
        let (tmp, config) = project();
        let notes = project_notes_file(config.path(), tmp.path());
        append_in(config.path(), tmp.path(), "just a plain note").unwrap();
        append_in(config.path(), tmp.path(), "another plain note").unwrap();

        let content = std::fs::read_to_string(&notes).unwrap();
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
        let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "# pinned content\n- entry one").unwrap();

        let first = load_in(tmp.path(), None).unwrap();
        assert!(first.contains("entry one"));

        // Hammer the cache. Pre-fix this would have hit disk N times.
        // The output must stay byte-identical to the first read because
        // nothing on disk changed and the cache must serve every call.
        for _ in 0..10 {
            let again = load_in(tmp.path(), None).unwrap();
            assert_eq!(again, first, "cache must serve identical bytes");
        }

        // Snapshot the cache entry directly to confirm we recorded
        // exactly one entry for this canonical path (i.e. didn't
        // re-insert on every call).
        let canonical = path.canonicalize().unwrap();
        let map = cache().lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.get(&canonical).expect("path must be cached");
        assert_eq!(entry.content, "# pinned content\n- entry one");
        assert_eq!(entry.len, std::fs::metadata(&path).unwrap().len());
    }

    /// Cache invalidation via `append`: writing through the public
    /// `append` API touches mtime, so the next `load` must detect
    /// the change and refresh.
    #[test]
    fn test_cache_invalidates_on_append() {
        let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache_for_tests();
        let (tmp, config) = project();
        write(
            &project_notes_file(config.path(), tmp.path()),
            "## Initial\n- one\n",
        );

        let before = load_in(tmp.path(), Some(config.path())).unwrap();
        assert!(before.contains("- one"));
        assert!(!before.contains("- two"));

        // Sleep just enough to guarantee a mtime tick on filesystems
        // with 1-second resolution (HFS+). Without this, an immediate
        // append could land in the same second AND happen to keep the
        // same len (impossible here — we're growing — but the sleep
        // is the principled defense).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        append_in(config.path(), tmp.path(), "- two").unwrap();

        let after = load_in(tmp.path(), Some(config.path())).unwrap();
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
        let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache_for_tests();
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CLAUDE.md");
        std::fs::write(&path, "short").unwrap();

        let before = load_in(tmp.path(), None).unwrap();
        assert!(before.contains("short"));

        // Overwrite with a larger payload. Even on coarse-mtime
        // filesystems the len delta forces a cache refresh.
        std::fs::write(&path, "this is much longer content than before").unwrap();
        let after = load_in(tmp.path(), None).unwrap();
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
        let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_cache_for_tests();
        let proj_a = TempDir::new().unwrap();
        let proj_b = TempDir::new().unwrap();
        std::fs::write(proj_a.path().join("CLAUDE.md"), "alpha-content").unwrap();
        std::fs::write(proj_b.path().join("CLAUDE.md"), "beta-content").unwrap();

        let a = load_in(proj_a.path(), None).unwrap();
        let b = load_in(proj_b.path(), None).unwrap();

        assert!(a.contains("alpha-content"));
        assert!(!a.contains("beta-content"));
        assert!(b.contains("beta-content"));
        assert!(!b.contains("alpha-content"));

        // Both paths are now cached independently. Toggle-load to
        // confirm neither cache entry was clobbered by the other.
        assert_eq!(load_in(proj_a.path(), None).unwrap(), a);
        assert_eq!(load_in(proj_b.path(), None).unwrap(), b);
    }

    /// Concurrent loads on the same project: the cache must serve
    /// all of them with at most ONE disk read. Models the bug-review
    /// scenario directly — parallel sub-agent fan-out hammering
    /// `memory::load` from N tokio tasks.
    #[test]
    fn test_concurrent_loads_share_cache() {
        let _guard = MEMORY_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
                        let c = load_in(&pr, None).unwrap();
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
        let map = cache().lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            map.contains_key(&canonical),
            "cache must contain entry for {canonical:?}"
        );
    }
}
