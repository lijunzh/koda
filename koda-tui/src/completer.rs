//! Tab completion for TUI input.
//!
//! Handles two completion modes:
//! - **Slash commands**: `/d` → `/diff`, `/diff commit`, `/diff review`
//!   (data + matching live in [`crate::composer::slash_popup`])
//! - **@file paths**: `explain @src/m` → `explain @src/main.rs`, found by
//!   the server's `fuzzyFileSearch` ([`crate::file_search`])
//!
//! Both modes share the same `InputCompleter` cycling state machine
//! (matches/idx/token) so Tab cycles through candidates regardless of
//! mode.

use crate::file_search::FileSearch;

/// Re-export so existing call sites (and external doc-tests) can keep
/// referring to `crate::completer::SLASH_COMMANDS`. The canonical
/// definition moved to [`crate::composer::slash_popup`] in #1187.
pub use crate::composer::slash_popup::SLASH_COMMANDS;

/// Unified Tab-completion for slash commands and @file paths.
#[derive(Default)]
pub struct InputCompleter {
    /// Current completion matches.
    matches: Vec<String>,
    /// Index into `matches` for cycling.
    idx: usize,
    /// The token being completed (to detect changes).
    token: String,
    /// The project's file search, for @file paths; none, none offered.
    files: Option<FileSearch>,
    /// Cached model names for `/model` completion.
    model_names: Vec<String>,
}

impl InputCompleter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Complete @file paths with `files`.
    pub(crate) fn with_file_search(files: FileSearch) -> Self {
        Self {
            files: Some(files),
            ..Self::new()
        }
    }

    /// Update the cached model names (call after provider switch or model list fetch).
    pub fn set_model_names(&mut self, names: Vec<String>) {
        self.model_names = names;
    }

    /// Attempt to complete the current input text.
    ///
    /// Returns `Some(replacement_text)` with the full input line replaced,
    /// or `None` if no completion is available.
    /// Repeated calls cycle through matches. An @file path waits for the
    /// server's search.
    pub async fn complete(&mut self, current_text: &str) -> Option<String> {
        let trimmed = current_text.trim_end();

        // Slash command completion: input starts with /
        if trimmed.starts_with('/') {
            // /model <partial> → complete model names
            if let Some(partial) = trimmed.strip_prefix("/model ") {
                return self.complete_model(partial);
            }
            return self.complete_slash(trimmed);
        }

        // @file completion: find the last @token in the input
        if let Some(at_pos) = find_last_at_token(trimmed) {
            let partial = &trimmed[at_pos + 1..]; // after @
            let prefix = &trimmed[..at_pos]; // everything before @
            return self.complete_file(prefix, partial).await;
        }

        self.reset();
        None
    }

    /// Reset completion state (call on non-Tab keystrokes).
    pub fn reset(&mut self) {
        self.matches.clear();
        self.idx = 0;
        self.token.clear();
    }

    // ── Slash command completion ─────────────────────────────

    fn complete_slash(&mut self, trimmed: &str) -> Option<String> {
        // Rebuild matches if the token changed. Matching itself lives in
        // `composer::slash_popup::matches_for` (#1187) so the slash table
        // is the single source of truth for both Tab-completion and the
        // auto-dropdown.
        if trimmed != self.token && !self.matches.iter().any(|m| m == trimmed) {
            self.token = trimmed.to_string();
            self.matches = crate::composer::slash_popup::matches_for(trimmed);
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let result = self.matches[self.idx].clone();
        self.idx = (self.idx + 1) % self.matches.len();
        Some(result)
    }

    // ── /model name completion ──────────────────────────────

    fn complete_model(&mut self, partial: &str) -> Option<String> {
        let token_key = format!("/model {partial}");

        if token_key != self.token {
            self.token = token_key;
            // The session's provider's aliases and model ids, from
            // `model/list`: another provider's model is a new session.
            self.matches = self
                .model_names
                .iter()
                .filter(|name| name.contains(partial) && name.as_str() != partial)
                .map(|name| format!("/model {name}"))
                .collect();
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let result = self.matches[self.idx].clone();
        self.idx = (self.idx + 1) % self.matches.len();
        Some(result)
    }

    // ── @file path completion ────────────────────────────────

    async fn complete_file(&mut self, prefix: &str, partial: &str) -> Option<String> {
        // Check if the partial is already one of our matches (user is cycling)
        let is_cycling = !self.matches.is_empty() && self.matches.iter().any(|m| m == partial);

        if !is_cycling {
            self.token = format!("@{partial}");
            self.matches = match &self.files {
                Some(files) => files.search(partial).await,
                None => Vec::new(),
            };
            self.idx = 0;
        }

        if self.matches.is_empty() {
            return None;
        }

        let path = &self.matches[self.idx];
        self.idx = (self.idx + 1) % self.matches.len();

        // Rebuild full input: prefix + @completed_path
        Some(format!("{prefix}@{path}"))
    }
}

// ── Helpers ─────────────────────────────────────────────────

/// Find the byte position of the last `@` that starts a file reference.
///
/// An `@` counts as a file reference if it's preceded by whitespace
/// or is at the start of the input (not an email address).
///
/// # Examples
///
/// ```ignore
/// // pub(crate): completer is not importable from a doc-test binary; illustrative only.
/// use koda_cli::completer::find_last_at_token;
///
/// // Leading @ at position 0
/// assert_eq!(find_last_at_token("@file.rs"), Some(0));
///
/// // @ after a space
/// assert_eq!(find_last_at_token("explain @src/main.rs"), Some(8));
///
/// // Email address — no space before @, so it is NOT treated as a file ref
/// assert_eq!(find_last_at_token("user@example.com"), None);
///
/// // Returns the LAST @ in multi-@ input
/// assert_eq!(find_last_at_token("@a @b"), Some(3));
///
/// // No @ at all
/// assert_eq!(find_last_at_token("just a normal prompt"), None);
/// ```
/// Where the `$name` skill mention being typed at the end of `text` starts:
/// a `$` at the start or after whitespace, followed only by name
/// characters, as Codex's `$` picker opens.
pub fn find_last_dollar_token(text: &str) -> Option<usize> {
    let start = text.rfind('$')?;
    let before_ok = start == 0 || matches!(text.as_bytes()[start - 1], b' ' | b'\n');
    // Codex's name characters, as the host reads `$name`.
    let name_ok = text[start + 1..]
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':'));
    (before_ok && name_ok).then_some(start)
}

/// The skills whose names match `partial`, best first: a prefix before a
/// fuzzy match, then by name. All of them for an empty `partial`.
pub fn skill_matches<'a>(
    skills: &'a [koda_client::protocol::skills::SkillMetadata],
    partial: &str,
) -> Vec<&'a koda_client::protocol::skills::SkillMetadata> {
    let mut scored: Vec<(i32, &koda_client::protocol::skills::SkillMetadata)> = skills
        .iter()
        .filter_map(|skill| {
            if partial.is_empty() {
                return Some((0, skill));
            }
            fuzzy_score(&partial.to_lowercase(), &skill.name.to_lowercase())
                .map(|score| (score, skill))
        })
        .collect();
    scored.sort_by(|(a, x), (b, y)| b.cmp(a).then_with(|| x.name.cmp(&y.name)));
    scored.into_iter().map(|(_, skill)| skill).collect()
}

pub fn find_last_at_token(text: &str) -> Option<usize> {
    for (i, c) in text.char_indices().rev() {
        if c == '@' && (i == 0 || matches!(text.as_bytes()[i - 1], b' ' | b'\n')) {
            return Some(i);
        }
    }
    None
}

/// Fuzzy subsequence scoring.
///
/// Returns `Some(score)` if all chars of `query` appear in `target` in order.
/// Higher score = better match.
///
/// Scoring (nucleo-inspired, matching CC's `native-ts/file-index`):
/// - Base: +1 per matched char
/// - Prefix / first char at pos 0: +100
/// - Consecutive chars: +10
/// - After separator (`_`, `-`, `.`, `/`): +5
/// - camelCase transition (lower→upper): +6
/// - Gap penalty: −3 (start) + −1 per additional gap char
///
/// `query` must be lowercased. `target` is **original case** so camelCase
/// transitions can be detected; character comparison is case-insensitive.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    let query_chars: Vec<char> = query.chars().collect();
    let target_chars: Vec<char> = target.chars().collect();

    let mut qi = 0;
    let mut score: i32 = 0;
    let mut prev_match_pos: Option<usize> = None;

    for (ti, &tc) in target_chars.iter().enumerate() {
        if qi < query_chars.len() && tc.to_ascii_lowercase() == query_chars[qi] {
            score += 1;

            // Bonus: prefix match
            if qi == 0 && ti == 0 {
                score += 100;
            }

            // Bonus: consecutive match
            if ti > 0 && prev_match_pos == Some(ti - 1) {
                score += 10;
            }

            // Bonus: after separator
            if ti > 0 && matches!(target_chars[ti - 1], '_' | '-' | '.' | '/') {
                score += 5;
            }

            // Bonus: camelCase transition (previous char lowercase, current uppercase)
            if ti > 0 && target_chars[ti - 1].is_ascii_lowercase() && tc.is_ascii_uppercase() {
                score += 6;
            }

            // Penalty: gap between consecutive matches
            if let Some(prev) = prev_match_pos {
                let gap = ti - prev - 1;
                if gap > 0 {
                    score -= 3 + gap as i32; // start + extension
                }
            }

            prev_match_pos = Some(ti);
            qi += 1;
        }
    }

    if qi == query_chars.len() {
        Some(score)
    } else {
        None // Not all query chars matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    /// A completer whose @file paths come from a Koda host for `root`;
    /// the host runs while the returned guard lives.
    async fn with_files(root: &Path) -> (InputCompleter, crate::file_search::tests::Host) {
        let host = crate::file_search::tests::Host::start(root).await;
        (InputCompleter::with_file_search(host.files(root)), host)
    }

    // ── Slash command tests ─────────────────────────────────

    #[tokio::test]
    async fn test_complete_slash_d() {
        let mut c = InputCompleter::new();
        let first = c.complete("/d").await;
        assert!(first.is_some());
        assert!(first.unwrap().starts_with("/d"));
    }

    #[tokio::test]
    async fn test_complete_cycles() {
        let mut c = InputCompleter::new();
        let a = c.complete("/d").await;
        let b = c.complete("/d").await;
        assert!(a.is_some());
        assert!(b.is_some());
    }

    #[tokio::test]
    async fn test_no_match() {
        let mut c = InputCompleter::new();
        assert!(c.complete("/zzz").await.is_none());
    }

    #[tokio::test]
    async fn test_non_slash_no_at_returns_none() {
        let mut c = InputCompleter::new();
        assert!(c.complete("hello").await.is_none());
    }

    #[tokio::test]
    async fn test_exact_match_no_complete() {
        let mut c = InputCompleter::new();
        assert!(c.complete("/exit").await.is_none());
    }

    // ── @file completion tests ───────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_completes() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(tmp.path().join("mod.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        let result = c.complete("explain @m").await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.starts_with("explain @m"), "got: {text}");
        assert!(
            text.contains("main.rs") || text.contains("mod.rs"),
            "got: {text}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_in_subdir() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "").unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        let result = c.complete("@src/l").await;
        assert_eq!(result, Some("@src/lib.rs".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_dir_gets_trailing_slash() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        let result = c.complete("@s").await;
        assert_eq!(result, Some("@src/".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_cycles() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("alpha.rs"), "").unwrap();
        fs::write(tmp.path().join("beta.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // First Tab: input is "@rs" → returns first match
        let a = c.complete("@rs").await.unwrap();
        // Second Tab: input is now the completed text (e.g., "@alpha.rs")
        let b = c.complete(&a).await.unwrap();
        assert_ne!(a, b, "should cycle through different files");
        // Third Tab: should cycle back
        let c_result = c.complete(&b).await.unwrap();
        assert_eq!(c_result, a, "should cycle back to first");
        assert_eq!(c_result, a, "should cycle back to first");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_offers_hidden_files() {
        // As Codex's search: hidden files too; a repository's `.gitignore`
        // decides what's left out.
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join(".hidden"), "").unwrap();
        fs::write(tmp.path().join("visible.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        assert_eq!(c.complete("@hid").await, Some("@.hidden".to_string()));
        c.reset();
        assert_eq!(c.complete("@vis").await, Some("@visible.rs".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_ignores_what_git_ignores() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(tmp.path().join("target/debug/build.log"), "").unwrap();
        fs::write(tmp.path().join("build.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        assert_eq!(c.complete("@build").await, Some("@build.rs".to_string()));
        let again = c.complete("@build.rs").await;
        assert_eq!(again, Some("@build.rs".to_string()), "the only match");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_empty_query_offers_nothing() {
        // As Codex's: an empty query searches nothing.
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "").unwrap();
        let (mut c, _host) = with_files(tmp.path()).await;
        assert_eq!(c.complete("@").await, None);
    }

    #[tokio::test]
    async fn test_at_file_without_a_search_offers_nothing() {
        let mut c = InputCompleter::new();
        assert_eq!(c.complete("@main").await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_case_insensitive() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("Makefile"), "").unwrap();
        fs::write(tmp.path().join("README.md"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        let result = c.complete("@make").await;
        assert_eq!(result, Some("@Makefile".to_string()));

        c.reset();
        let result = c.complete("@read").await;
        assert_eq!(result, Some("@README.md".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_preserves_prefix_text() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        let result = c.complete("review this @c").await;
        assert_eq!(result, Some("review this @config.toml".to_string()));
    }

    // ── /model completion tests ──────────────────────────────

    #[tokio::test]
    async fn test_model_complete() {
        let mut c = InputCompleter::new();
        c.set_model_names(vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "gpt-3.5-turbo".into(),
        ]);
        let result = c.complete("/model gpt-4").await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.starts_with("/model gpt-4"), "got: {text}");
    }

    #[tokio::test]
    async fn test_model_complete_cycles() {
        let mut c = InputCompleter::new();
        c.set_model_names(vec!["gpt-4o".into(), "gpt-4o-mini".into()]);
        let a = c.complete("/model gpt").await;
        let b = c.complete("/model gpt").await;
        assert!(a.is_some());
        assert!(b.is_some());
        assert_ne!(a, b, "should cycle through models");
    }

    #[tokio::test]
    async fn test_model_no_names_returns_none() {
        let mut c = InputCompleter::new();
        // No model names from the host yet: nothing to complete.
        assert!(c.complete("/model gpt").await.is_none());
    }

    #[tokio::test]
    async fn test_model_no_match_returns_none() {
        let mut c = InputCompleter::new();
        // "zzz" matches no model names
        assert!(c.complete("/model zzz").await.is_none());
    }

    #[tokio::test]
    async fn test_model_substring_match() {
        let mut c = InputCompleter::new();
        c.set_model_names(vec![
            "claude-opus".into(),
            "claude-sonnet-5".into(),
            "claude-opus-5".into(),
        ]);
        let result = c.complete("/model opus").await;
        // "opus" matches the alias "claude-opus" and the id "claude-opus-5",
        // both from `model/list`
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("opus"), "got: {text}");
    }

    // ── Helper tests ────────────────────────────────────────

    #[test]
    fn test_find_last_at_token() {
        assert_eq!(find_last_at_token("@file"), Some(0));
        assert_eq!(find_last_at_token("explain @file"), Some(8));
        assert_eq!(find_last_at_token("email@domain"), None); // no space before @
        assert_eq!(find_last_at_token("a @b @c"), Some(5)); // last @
        assert_eq!(find_last_at_token("no at here"), None);
        // @ after newline (multi-line input via Alt+Enter)
        assert_eq!(find_last_at_token("line1\n@file"), Some(6));
        assert_eq!(find_last_at_token("a\nb\n@c"), Some(4));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_after_newline() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("config.toml"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // Simulate multi-line input: first line + newline + @partial
        let result = c.complete("explain this\n@c").await;
        assert_eq!(result, Some("explain this\n@config.toml".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_at_file_traversal_blocked() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("safe.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // Only the project is searched: nothing above it
        let result = c.complete("@../../etc/").await;
        assert!(result.is_none(), "traversal should be blocked");
    }

    // ── Fuzzy matching tests ────────────────────────────────

    #[test]
    fn test_fuzzy_score_basic() {
        // Exact prefix → high score
        assert!(fuzzy_score("main", "main.rs").unwrap() > 100);
        // Subsequence match
        assert!(fuzzy_score("mrs", "main.rs").is_some());
        // No match
        assert!(fuzzy_score("xyz", "main.rs").is_none());
    }

    #[test]
    fn test_fuzzy_score_prefix_wins() {
        let prefix = fuzzy_score("ma", "main.rs").unwrap();
        let fuzzy = fuzzy_score("ma", "format.rs").unwrap();
        assert!(prefix > fuzzy, "prefix {prefix} should beat fuzzy {fuzzy}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fuzzy_at_file() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "").unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("config.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // "mrs" → should fuzzy-match main.rs (m...r.s)
        let result = c.complete("@mrs").await;
        assert_eq!(result, Some("@main.rs".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fuzzy_cargo_toml() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("config.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // "ctml" → fuzzy-match Cargo.toml (c...t..m.l)
        let result = c.complete("@ctml").await;
        assert_eq!(result, Some("@Cargo.toml".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fuzzy_prefix_ranked_first() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "").unwrap();
        fs::write(tmp.path().join("format.rs"), "").unwrap();

        let (mut c, _host) = with_files(tmp.path()).await;
        // "m" → main.rs should come before format.rs (prefix match wins)
        let result = c.complete("@m").await;
        assert_eq!(result, Some("@main.rs".to_string()));
    }

    // ── Gap penalty tests ──────────────────────────────────

    #[test]
    fn test_gap_penalty_tight_beats_scattered() {
        // "mrs": main.rs has gap=1 (m-a-i-n-.-r-s), scattered has large gaps
        let tight = fuzzy_score("mrs", "main.rs").unwrap();
        let scattered = fuzzy_score("mrs", "my_really_long_script.rs").unwrap();
        assert!(
            tight > scattered,
            "tight {tight} should beat scattered {scattered}"
        );
    }

    #[test]
    fn test_gap_penalty_consecutive_no_penalty() {
        // Consecutive chars should get bonus, not penalty
        let consec = fuzzy_score("mai", "main.rs").unwrap();
        let gapped = fuzzy_score("mai", "m_a_i.rs").unwrap();
        assert!(
            consec > gapped,
            "consecutive {consec} should beat gapped {gapped}"
        );
    }

    // ── camelCase bonus tests ──────────────────────────────

    #[test]
    fn test_camel_case_bonus() {
        // "dm" at camelCase boundary (D→M) should score higher
        let camel = fuzzy_score("dm", "DropdownMenu").unwrap();
        let flat = fuzzy_score("dm", "random_dm_file").unwrap();
        assert!(camel > flat, "camelCase {camel} should beat flat {flat}");
    }

    #[test]
    fn a_dollar_mention_opens_where_codexs_picker_does() {
        assert_eq!(find_last_dollar_token("$"), Some(0));
        assert_eq!(find_last_dollar_token("use $che"), Some(4));
        assert_eq!(find_last_dollar_token("use $lint:fi"), Some(4));
        assert_eq!(find_last_dollar_token("a\n$x"), Some(2));
        assert_eq!(find_last_dollar_token("use $checklist "), None, "done");
        assert_eq!(find_last_dollar_token("cost$5"), None, "inside a word");
        assert_eq!(find_last_dollar_token("$a.b"), None);
        assert_eq!(find_last_dollar_token("no mention"), None);
    }

    #[test]
    fn skills_match_by_name_prefix_first() {
        use koda_client::protocol::skills::{SkillMetadata, SkillScope};
        let skill = |name: &str| SkillMetadata {
            name: name.into(),
            description: String::new(),
            path: None,
            scope: SkillScope::Repo,
            enabled: true,
            user_invocable: true,
        };
        let skills = [skill("review-pr"), skill("deploy"), skill("pre-review")];
        let names = |partial: &str| -> Vec<String> {
            skill_matches(&skills, partial)
                .into_iter()
                .map(|s| s.name.clone())
                .collect()
        };
        assert_eq!(names(""), vec!["deploy", "pre-review", "review-pr"]);
        assert_eq!(names("rev").first().map(String::as_str), Some("review-pr"));
        assert_eq!(names("dep"), vec!["deploy"]);
        assert!(names("zzz").is_empty());
    }
}
