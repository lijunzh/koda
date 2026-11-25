//! `fuzzyFileSearch`: the files and folders under a client's roots whose
//! paths fuzzily match its query, for `@` mentions, so no client walks the
//! file system itself.
//!
//! As Codex's: the walk is `ignore`'s, on up to 12 threads, with hidden
//! entries included and links followed; `.gitignore` counts only inside a
//! git repository (`require_git`). Paths are matched with `nucleo`'s
//! path-aware scoring, ignoring case. A one-word query that names no folder
//! ranks a file whose own name matches above one that matches only by its
//! folders (an exact name highest), and lets the best folder of exactly
//! that name compete with them. At most 50 matches come back, best first and
//! equal scores by path, with the matched positions for highlighting. A new
//! request with the same cancellation token cancels the one in flight, which
//! answers with no files.
//!
//! ## Provenance
//!
//! Ported from Codex at upstream commit `06f334aa4b` (codex `main` as of
//! 2026-11-25): the walk, the matching and the ranking from
//! `codex-rs/file-search/src/lib.rs` and `src/snapshot.rs`; the request's
//! handling, its limits and its order from
//! `codex-rs/app-server/src/fuzzy_file_search.rs` and
//! `src/request_processors/search.rs`; the tests from
//! `codex-rs/file-search/src/lib_tests.rs`.
//!
//! Changed: Codex runs its one-shot search on the streaming session it
//! built for `fuzzyFileSearch/sessionStart`, feeding a `Nucleo` worker and
//! ranking its snapshots in time slices. Koda has no streaming variant, so
//! each walker thread scores the paths it finds with its own `nucleo`
//! matcher and the ranking runs once the walk is done, in the order a
//! `Nucleo` snapshot gives (score, then shorter path; Koda breaks Nucleo's
//! remaining tie, arrival order, by path, so the result is the same on every
//! run). A cancelled search answers with no files rather than what it had
//! found, the exclude globs and the switch to ignore `.gitignore` are left
//! out (nothing in Koda sets them), and the roots must be absolute: one
//! shared server serves clients in different folders, so a relative root
//! would mean the server's.
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` for the full license text.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ignore::{WalkBuilder, WalkState};
use koda_protocol::rpc::ErrorObject;
use koda_protocol::search::{
    FuzzyFileSearchMatchType, FuzzyFileSearchParams, FuzzyFileSearchResponse, FuzzyFileSearchResult,
};
use nucleo::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::session::{internal, invalid};

/// The most matches an answer holds.
const MATCH_LIMIT: usize = 50;

/// The most threads a search walks on.
const MAX_THREADS: usize = 12;

/// A single `nucleo` atom scores at most `u16::MAX`; a name bonus lifts a
/// match above every score of the group below it.
const NAME_BONUS: u32 = u16::MAX as u32 + 1;

/// How many entries a walker thread takes between looks at the cancel flag.
const CHECK_INTERVAL: usize = 1024;

/// The searches in flight, by cancellation token.
#[derive(Default)]
pub(crate) struct PendingSearches(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl PendingSearches {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<AtomicBool>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Answer `fuzzyFileSearch`.
pub(crate) async fn fuzzy_file_search(
    pending: &PendingSearches,
    params: FuzzyFileSearchParams,
) -> Result<FuzzyFileSearchResponse, ErrorObject> {
    let FuzzyFileSearchParams {
        query,
        roots,
        cancellation_token,
    } = params;
    if let Some(root) = roots.iter().find(|root| !Path::new(root).is_absolute()) {
        return Err(invalid(format!(
            "fuzzyFileSearch roots must be absolute paths: {root:?}"
        )));
    }

    // A request with a token cancels the one in flight with the same token.
    let cancel = Arc::new(AtomicBool::new(false));
    if let Some(token) = &cancellation_token
        && let Some(earlier) = pending.lock().insert(token.clone(), Arc::clone(&cancel))
    {
        earlier.store(true, Ordering::Relaxed);
    }

    let files = if query.is_empty() || roots.is_empty() {
        Ok(Vec::new())
    } else {
        let cancel = Arc::clone(&cancel);
        tokio::task::spawn_blocking(move || {
            let roots = roots.iter().map(PathBuf::from).collect();
            search(&query, roots, MATCH_LIMIT, threads(), &cancel)
        })
        .await
        .map_err(|e| internal(format!("fuzzyFileSearch failed: {e}")))
    };

    if let Some(token) = cancellation_token {
        let mut pending = pending.lock();
        if pending
            .get(&token)
            .is_some_and(|current| Arc::ptr_eq(current, &cancel))
        {
            pending.remove(&token);
        }
    }

    let mut files: Vec<FuzzyFileSearchResult> = files?
        .into_iter()
        .map(|found| FuzzyFileSearchResult {
            root: found.root.to_string_lossy().into_owned(),
            file_name: Path::new(&found.path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| found.path.clone()),
            path: found.path,
            match_type: match found.match_type {
                MatchType::File => FuzzyFileSearchMatchType::File,
                MatchType::Directory => FuzzyFileSearchMatchType::Directory,
            },
            score: found.score,
            indices: Some(found.indices),
        })
        .collect();
    files.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    Ok(FuzzyFileSearchResponse { files })
}

/// The threads a search walks on: one per core, at most [`MAX_THREADS`].
fn threads() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .clamp(1, MAX_THREADS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchType {
    File,
    Directory,
}

/// One match: its root, its path below it, how well it matches and where.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMatch {
    root: PathBuf,
    path: String,
    match_type: MatchType,
    score: u32,
    indices: Vec<u32>,
}

/// An entry whose path matches, as a walker thread scored it.
struct Candidate {
    root_idx: usize,
    path: String,
    /// Its length in characters, Nucleo's tie-break.
    chars: usize,
    match_type: MatchType,
    /// The whole path's score.
    path_score: u32,
    /// The name's score against the query's one word, when names rank.
    name_score: Option<u16>,
    /// Whether the name is as long as the query's one word.
    exact_length: bool,
}

/// What every walker thread shares.
struct Walk<'a> {
    pattern: &'a Pattern,
    /// The query's one word, when names rank ([`ranks_names`]).
    name: Option<&'a Atom>,
    roots: &'a [PathBuf],
    /// For a root nested in another, the paths already scored below it.
    seen: Vec<Option<Mutex<HashSet<String>>>>,
    cancel: &'a AtomicBool,
    found: Mutex<Vec<Candidate>>,
}

/// The best `limit` matches for `query` under `roots`, best first, or none
/// once `cancel` is set.
fn search(
    query: &str,
    roots: Vec<PathBuf>,
    limit: usize,
    threads: usize,
    cancel: &AtomicBool,
) -> Vec<FileMatch> {
    let Some((first, others)) = roots.split_first() else {
        return Vec::new();
    };
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let name = ranks_names(&pattern, query).then(|| &pattern.atoms[0]);

    let mut builder = WalkBuilder::new(first);
    let mut walked = HashSet::from([first.as_path()]);
    for root in others {
        if walked.insert(root.as_path()) {
            builder.add(root);
        }
    }
    // Keep both walks for nested roots: ignore rules can expose different
    // entries. Only those subtrees need to remember what they've scored.
    let seen = roots
        .iter()
        .map(|root| {
            root.ancestors()
                .skip(1)
                .any(|parent| walked.contains(parent))
                .then(|| Mutex::new(HashSet::new()))
        })
        .collect();
    builder
        .threads(threads)
        // Hidden entries too.
        .hidden(false)
        // Follow links to search what they point to.
        .follow_links(true)
        // Git's own rule: a `.gitignore` above the repository, or outside
        // any repository, doesn't count.
        .require_git(true);

    let walk = Walk {
        pattern: &pattern,
        name,
        roots: &roots,
        seen,
        cancel,
        found: Mutex::new(Vec::new()),
    };
    builder.build_parallel().run(|| {
        let mut thread = WalkerThread {
            walk: &walk,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            buf: Vec::new(),
            found: Vec::new(),
            visited: 0,
        };
        Box::new(move |entry| thread.visit(entry))
    });
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let found = walk
        .found
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    rank(found, &pattern, name.is_some(), &roots, limit)
}

/// Whether a filename's own match counts above its path's, as Codex ranks
/// it: a query of one fuzzy word, not negated, naming no folder.
fn ranks_names(pattern: &Pattern, query: &str) -> bool {
    matches!(pattern.atoms.as_slice(), [atom] if atom.kind == AtomKind::Fuzzy && !atom.negative)
        && !query.contains(['/', '\\'])
}

/// One walker thread: its own matcher, and what it found, handed over when
/// the walk is done with it.
struct WalkerThread<'a, 'w> {
    walk: &'w Walk<'a>,
    matcher: Matcher,
    buf: Vec<char>,
    found: Vec<Candidate>,
    visited: usize,
}

impl WalkerThread<'_, '_> {
    fn visit(&mut self, entry: Result<ignore::DirEntry, ignore::Error>) -> WalkState {
        self.visited += 1;
        if self.visited.is_multiple_of(CHECK_INTERVAL) && self.walk.cancel.load(Ordering::Relaxed) {
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let Some((root_idx, path)) = relative_path(entry.path(), self.walk.roots) else {
            return WalkState::Continue;
        };
        if let Some(seen) = &self.walk.seen[root_idx]
            && !seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.to_string())
        {
            return WalkState::Continue;
        }
        let match_type = match entry.file_type() {
            Some(kind) if kind.is_dir() => MatchType::Directory,
            _ => MatchType::File,
        };
        let haystack = Utf32Str::new(path, &mut self.buf);
        let Some(path_score) = self.walk.pattern.score(haystack, &mut self.matcher) else {
            return WalkState::Continue;
        };
        let (name_score, exact_length) = match self.walk.name {
            Some(name) => {
                let basename = basename(haystack);
                let exact_length = basename.len() == name.needle_text().len();
                // A folder's name counts only when it's exactly the word.
                let score = (match_type == MatchType::File || exact_length)
                    .then(|| name.score(basename, &mut self.matcher))
                    .flatten();
                (score, exact_length)
            }
            None => (None, false),
        };
        self.found.push(Candidate {
            root_idx,
            chars: haystack.len(),
            path: path.to_string(),
            match_type,
            path_score,
            name_score,
            exact_length,
        });
        WalkState::Continue
    }
}

impl Drop for WalkerThread<'_, '_> {
    fn drop(&mut self) {
        self.walk
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .append(&mut self.found);
    }
}

/// The root `path` is under, the deepest when roots nest, and the path
/// relative to it; `None` for a path that isn't UTF-8.
fn relative_path<'a>(path: &'a Path, roots: &[PathBuf]) -> Option<(usize, &'a str)> {
    let mut best: Option<(usize, &Path)> = None;
    for (idx, root) in roots.iter().enumerate() {
        if let Ok(relative) = path.strip_prefix(root) {
            let depth = root.components().count();
            match best {
                Some((best_idx, _)) if roots[best_idx].components().count() >= depth => {}
                _ => best = Some((idx, relative)),
            }
        }
    }
    let (root_idx, relative) = best?;
    relative.to_str().map(|path| (root_idx, path))
}

/// Order the candidates and take the best `limit`, as Codex ranks a
/// `Nucleo` snapshot, then find each one's matched positions.
fn rank(
    mut found: Vec<Candidate>,
    pattern: &Pattern,
    rank_names: bool,
    roots: &[PathBuf],
    limit: usize,
) -> Vec<FileMatch> {
    // Nucleo's order: score, then the shorter path.
    found.sort_by(|a, b| {
        b.path_score
            .cmp(&a.path_score)
            .then(a.chars.cmp(&b.chars))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut scored: Vec<(u32, bool, Candidate)> = if rank_names {
        let mut found_exact_folder = false;
        found
            .into_iter()
            .map(|candidate| {
                let score = match (candidate.match_type, candidate.name_score) {
                    (MatchType::File, Some(score)) => {
                        let bonus = if candidate.exact_length {
                            3 * NAME_BONUS
                        } else {
                            NAME_BONUS
                        };
                        bonus + u32::from(score)
                    }
                    // Keep the strongest folder of exactly that name. More
                    // of the same name compete by path, so they can't crowd
                    // out matching files.
                    (MatchType::Directory, Some(_)) if !found_exact_folder => {
                        found_exact_folder = true;
                        2 * NAME_BONUS + candidate.path_score
                    }
                    _ => candidate.path_score,
                };
                (score, score >= NAME_BONUS, candidate)
            })
            .collect()
    } else {
        found
            .into_iter()
            .map(|candidate| (candidate.path_score, false, candidate))
            .collect()
    };
    // Stable: equal scores keep Nucleo's order.
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    scored.truncate(limit);

    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut buf = Vec::new();
    scored
        .into_iter()
        .map(|(score, matched_name, candidate)| {
            let path = Utf32Str::new(&candidate.path, &mut buf);
            let haystack = if matched_name { basename(path) } else { path };
            let offset = (path.len() - haystack.len()) as u32;
            let mut indices = Vec::new();
            let _ = pattern.indices(haystack, &mut matcher, &mut indices);
            for index in &mut indices {
                *index += offset;
            }
            indices.sort_unstable();
            indices.dedup();
            FileMatch {
                root: roots[candidate.root_idx].clone(),
                path: candidate.path,
                match_type: candidate.match_type,
                score,
                indices,
            }
        })
        .collect()
}

/// A path's last component.
fn basename(path: Utf32Str<'_>) -> Utf32Str<'_> {
    let len = path
        .chars()
        .rev()
        .take_while(|c| !std::path::is_separator(*c))
        .count();
    path.slice(path.len() - len..)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run(query: &str, roots: Vec<PathBuf>, limit: usize) -> Vec<FileMatch> {
        search(query, roots, limit, 2, &AtomicBool::new(false))
    }

    fn paths(matches: &[FileMatch]) -> Vec<&str> {
        matches.iter().map(|m| m.path.as_str()).collect()
    }

    #[test]
    fn finds_files_and_folders_with_their_positions() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs/guides")).unwrap();
        fs::write(dir.path().join("docs/guides/intro.md"), "").unwrap();
        fs::write(dir.path().join("docs/readme.md"), "").unwrap();

        let found = run("guides", vec![dir.path().to_path_buf()], 20);
        let folder = found.iter().find(|m| m.path == "docs/guides").unwrap();
        assert_eq!(folder.match_type, MatchType::Directory);
        assert_eq!(folder.root, dir.path());
        assert_eq!(folder.indices, (5..11).collect::<Vec<u32>>());

        let found = run("intro", vec![dir.path().to_path_buf()], 20);
        assert_eq!(found[0].path, "docs/guides/intro.md");
        assert_eq!(found[0].match_type, MatchType::File);
        assert!(run("zzz", vec![dir.path().to_path_buf()], 20).is_empty());
    }

    // Codex's `matching_filenames_survive_the_path_result_limit`.
    #[test]
    fn matching_filenames_survive_the_path_result_limit() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("viewmodel");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("build.gradle.kts"), "").unwrap();
        fs::create_dir_all(root.path().join("feature/src")).unwrap();
        fs::write(root.path().join("feature/src/ChatViewModel.kt"), "").unwrap();
        let search = |query: &str, limit| run(query, vec![root.path().to_path_buf()], limit);

        for i in 0..80 {
            fs::create_dir_all(root.path().join(format!("parent-{i}/viewmodel"))).unwrap();
        }
        let hits = search("ViewModel", 2);
        assert_eq!(hits[0].match_type, MatchType::Directory);
        assert_eq!(hits[1].path, "feature/src/ChatViewModel.kt");
        assert_eq!(search("VM", 1)[0].path, "feature/src/ChatViewModel.kt");
        fs::create_dir(root.path().join("src")).unwrap();
        for i in 0..80 {
            fs::write(root.path().join(format!("source-{i}.rs")), "").unwrap();
        }
        assert_eq!(search("src", 1)[0].path, "src");
        fs::write(root.path().join("feature/src/src"), "").unwrap();
        assert_eq!(search("src", 1)[0].path, "feature/src/src");
        assert_eq!(
            search("viewmodel/", 1)[0].path,
            "viewmodel/build.gradle.kts"
        );
    }

    #[test]
    fn answers_at_most_the_limit_best_first() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MATCH_LIMIT + 20) {
            fs::write(dir.path().join(format!("file-{i:04}.txt")), "").unwrap();
        }
        let found = run("file", vec![dir.path().to_path_buf()], MATCH_LIMIT);
        assert_eq!(found.len(), MATCH_LIMIT);
        assert!(found.windows(2).all(|pair| pair[0].score >= pair[1].score));
    }

    // Codex's `overlapping_roots_share_result_slots_by_path`.
    #[test]
    fn overlapping_roots_find_each_path_once() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_path_buf();
        let child = parent.join("sub");
        fs::create_dir(&child).unwrap();
        fs::write(parent.join("target.txt"), "").unwrap();
        fs::write(child.join("target.txt"), "").unwrap();
        for roots in [
            vec![parent.clone(), child.clone(), parent.clone()],
            vec![child.clone(), parent.clone(), child.clone()],
        ] {
            let found = run("target", roots, 2);
            let mut found: Vec<_> = found.into_iter().map(|m| (m.root, m.path)).collect();
            found.sort();
            assert_eq!(
                found,
                vec![
                    (parent.clone(), "target.txt".to_string()),
                    (child.clone(), "target.txt".to_string()),
                ]
            );
        }
    }

    #[test]
    fn hidden_entries_are_found() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".config")).unwrap();
        fs::write(dir.path().join(".config/settings.toml"), "").unwrap();
        let found = run("settings", vec![dir.path().to_path_buf()], 20);
        assert_eq!(paths(&found), vec![".config/settings.toml"]);
    }

    // Codex's `run_classifies_followed_directory_symlink_as_directory`.
    #[cfg(unix)]
    #[test]
    fn a_linked_folder_counts_as_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("guides")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("guides"), dir.path().join("guides-link"))
            .unwrap();
        let found = run("guides-link", vec![dir.path().to_path_buf()], 20);
        assert!(
            found
                .iter()
                .any(|m| m.path == "guides-link" && m.match_type == MatchType::Directory)
        );
    }

    // Codex's `parent_gitignore_outside_repo_does_not_hide_repo_files`.
    #[test]
    fn a_gitignore_outside_a_repository_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();
        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{}").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{}").unwrap();
        fs::write(repo.join(".vscode/extensions.json"), "{}").unwrap();

        assert!(paths(&run("package", vec![repo.clone()], 20)).contains(&"package.json"));
        // No repository: the repo's own `.gitignore` doesn't count either.
        assert!(
            paths(&run("extensions.json", vec![repo.clone()], 20))
                .contains(&".vscode/extensions.json")
        );
    }

    // Codex's `git_repo_still_respects_local_gitignore_when_enabled`.
    #[test]
    fn a_repositorys_gitignore_is_honoured() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("home");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".vscode")).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(parent.join(".gitignore"), "*\n!.gitignore\n").unwrap();
        fs::write(
            repo.join(".gitignore"),
            ".vscode/*\n!.vscode/\n!.vscode/settings.json\n!package.json\n",
        )
        .unwrap();
        fs::write(repo.join("package.json"), "{}").unwrap();
        fs::write(repo.join(".vscode/settings.json"), "{}").unwrap();
        fs::write(repo.join(".vscode/extensions.json"), "{}").unwrap();

        assert!(paths(&run("package", vec![repo.clone()], 20)).contains(&"package.json"));
        assert!(
            !paths(&run("extensions.json", vec![repo.clone()], 20))
                .contains(&".vscode/extensions.json")
        );
        assert!(paths(&run("settings.json", vec![repo], 20)).contains(&".vscode/settings.json"));
    }

    #[test]
    fn only_the_roots_are_searched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(dir.path().join("secret.txt"), "").unwrap();
        fs::write(root.join("notes.txt"), "").unwrap();
        assert!(run("secret", vec![root.clone()], 20).is_empty());
        assert!(run("../secret", vec![root], 20).is_empty());
    }

    // Codex's `cancel_exits_run`.
    #[test]
    fn a_cancelled_search_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            fs::write(dir.path().join(format!("file-{i:04}.txt")), "").unwrap();
        }
        let cancelled = AtomicBool::new(true);
        let found = search("file-", vec![dir.path().to_path_buf()], 20, 2, &cancelled);
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn an_empty_query_or_no_roots_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "").unwrap();
        let pending = PendingSearches::default();
        let root = dir.path().display().to_string();
        let ask = |query: &str, roots: Vec<String>| FuzzyFileSearchParams {
            query: query.into(),
            roots,
            cancellation_token: None,
        };
        let empty = fuzzy_file_search(&pending, ask("", vec![root.clone()])).await;
        assert!(empty.unwrap().files.is_empty());
        let rootless = fuzzy_file_search(&pending, ask("main", Vec::new())).await;
        assert!(rootless.unwrap().files.is_empty());
        let found = fuzzy_file_search(&pending, ask("main", vec![root.clone()]))
            .await
            .unwrap();
        assert_eq!(found.files[0].path, "main.rs");
        assert_eq!(found.files[0].file_name, "main.rs");
        assert_eq!(found.files[0].root, root);
        assert_eq!(found.files[0].indices, Some(vec![0, 1, 2, 3]));
        let relative = fuzzy_file_search(&pending, ask("main", vec!["src".into()])).await;
        assert!(relative.is_err());
    }

    #[tokio::test]
    async fn a_new_request_with_the_same_token_cancels_the_old_one() {
        let pending = PendingSearches::default();
        let first = Arc::new(AtomicBool::new(false));
        pending.lock().insert("composer".into(), Arc::clone(&first));
        let other = Arc::new(AtomicBool::new(false));
        pending.lock().insert("other".into(), Arc::clone(&other));

        let dir = tempfile::tempdir().unwrap();
        let params = FuzzyFileSearchParams {
            query: "x".into(),
            roots: vec![dir.path().display().to_string()],
            cancellation_token: Some("composer".into()),
        };
        fuzzy_file_search(&pending, params).await.unwrap();
        assert!(
            first.load(Ordering::Relaxed),
            "the earlier search is cancelled"
        );
        assert!(!other.load(Ordering::Relaxed), "another token's isn't");
        // Done: its own entry is gone; the other search's stays.
        assert!(!pending.lock().contains_key("composer"));
        assert!(pending.lock().contains_key("other"));
    }
}
