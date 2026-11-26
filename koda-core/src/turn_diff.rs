//! The turn's edits as one unified diff, for `turn/diff/updated`.
//!
//! Ported from Codex's `TurnDiffTracker` (`codex-rs/core/src/turn_diff_tracker.rs`
//! at 80ea5901ad, Apache-2.0; see NOTICE). Like Codex's, it builds the diff
//! from the edits Koda applied, not by rereading the files: each file is
//! compared with its content when the turn first edited it, so a file edited
//! twice shows once, and an edit that is undone drops out. Koda's edits
//! never move a file, so Codex's rename pairing and its per-environment paths
//! are left out.

use std::collections::BTreeMap;
use std::time::Duration;

use sha1::{Digest, Sha1};
use similar::TextDiff;

const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DEV_NULL: &str = "/dev/null";
const REGULAR_FILE_MODE: &str = "100644";
/// As Codex's: normal edits finish well within this; a pathological one
/// falls back to a coarser diff that is still exact.
const DIFF_TIMEOUT: Duration = Duration::from_millis(100);

/// The edits one turn has applied, by path.
#[derive(Debug, Clone)]
pub struct TurnDiff {
    /// False once an edit couldn't be tracked: the diff would be wrong.
    valid: bool,
    /// Each file as the turn first found it; `None` if it didn't exist.
    baseline: BTreeMap<String, Option<String>>,
    /// Each file now; `None` if it no longer exists.
    current: BTreeMap<String, Option<String>>,
    diff: Option<String>,
}

impl Default for TurnDiff {
    fn default() -> Self {
        Self {
            valid: true,
            baseline: BTreeMap::new(),
            current: BTreeMap::new(),
            diff: None,
        }
    }
}

impl TurnDiff {
    /// An empty diff, for a new turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an applied edit to `path` (relative to the session's
    /// folder, `/`-separated): its content `before` (`None` if it didn't
    /// exist) and `after` (`None` if it was deleted).
    pub fn record(&mut self, path: &str, before: Option<&str>, after: Option<&str>) {
        if !self.valid {
            return;
        }
        self.baseline
            .entry(path.to_string())
            .or_insert_with(|| before.map(str::to_string));
        self.current
            .insert(path.to_string(), after.map(str::to_string));
        self.refresh();
    }

    /// An edit was applied that can't be tracked: from now on the turn has
    /// no diff, as Codex's.
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.diff = None;
    }

    /// The whole diff, or `None` when nothing differs.
    pub fn diff(&self) -> Option<&str> {
        self.diff.as_deref()
    }

    fn refresh(&mut self) {
        let mut all = String::new();
        for (path, before) in &self.baseline {
            let after = self.current.get(path).cloned().flatten();
            if let Some(diff) = render(path, before.as_deref(), after.as_deref()) {
                all.push_str(&diff);
                if !all.ends_with('\n') {
                    all.push('\n');
                }
            }
        }
        self.diff = (!all.is_empty()).then_some(all);
    }
}

/// One file's diff in git's format, or `None` if it didn't change.
fn render(path: &str, before: Option<&str>, after: Option<&str>) -> Option<String> {
    if before == after {
        return None;
    }
    let mut diff = format!("diff --git a/{path} b/{path}\n");
    match (before, after) {
        (None, Some(_)) => diff.push_str(&format!("new file mode {REGULAR_FILE_MODE}\n")),
        (Some(_), None) => diff.push_str(&format!("deleted file mode {REGULAR_FILE_MODE}\n")),
        _ => {}
    }
    let oid = |content: Option<&str>| content.map_or_else(|| ZERO_OID.to_string(), blob_oid);
    diff.push_str(&format!("index {}..{}\n", oid(before), oid(after)));

    let old_header = before.map_or_else(|| DEV_NULL.to_string(), |_| format!("a/{path}"));
    let new_header = after.map_or_else(|| DEV_NULL.to_string(), |_| format!("b/{path}"));
    let unified = TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(before.unwrap_or(""), after.unwrap_or(""))
        .unified_diff()
        .context_radius(3)
        .header(&old_header, &new_header)
        .to_string();
    diff.push_str(&unified);
    Some(diff)
}

/// Git's object id for a blob with this content.
fn blob_oid(content: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", content.len()).as_bytes());
    hasher.update(content.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edited_file_reads_as_git_diff_shows_it() {
        let mut turn = TurnDiff::new();
        turn.record("src/a.rs", Some("one\ntwo\n"), Some("one\n2\n"));
        let expected = format!(
            "diff --git a/src/a.rs b/src/a.rs\n\
             index {}..{}\n\
             --- a/src/a.rs\n\
             +++ b/src/a.rs\n\
             @@ -1,2 +1,2 @@\n one\n-two\n+2\n",
            blob_oid("one\ntwo\n"),
            blob_oid("one\n2\n"),
        );
        assert_eq!(turn.diff().unwrap(), expected);
    }

    /// The ids are git's: `git hash-object` of "hello\n".
    #[test]
    fn blob_ids_are_gits() {
        assert_eq!(
            blob_oid("hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn a_file_edited_twice_shows_once_against_its_first_content() {
        let mut turn = TurnDiff::new();
        turn.record("a.txt", Some("a\n"), Some("b\n"));
        turn.record("a.txt", Some("b\n"), Some("c\n"));
        let diff = turn.diff().unwrap();
        assert_eq!(diff.matches("diff --git").count(), 1);
        assert!(diff.contains("-a\n+c\n"), "{diff}");
    }

    #[test]
    fn an_undone_edit_leaves_no_diff() {
        let mut turn = TurnDiff::new();
        turn.record("a.txt", Some("a\n"), Some("b\n"));
        turn.record("a.txt", Some("b\n"), Some("a\n"));
        assert_eq!(turn.diff(), None);
    }

    #[test]
    fn a_new_file_is_added_from_dev_null() {
        let mut turn = TurnDiff::new();
        turn.record("new.txt", None, Some("hi\n"));
        let diff = turn.diff().unwrap();
        assert!(diff.contains("new file mode 100644\n"), "{diff}");
        assert!(diff.contains(&format!("index {ZERO_OID}..")), "{diff}");
        assert!(diff.contains("--- /dev/null\n+++ b/new.txt\n"), "{diff}");
        assert!(diff.ends_with("+hi\n"), "{diff}");
    }

    #[test]
    fn files_come_in_path_order() {
        let mut turn = TurnDiff::new();
        turn.record("b.txt", Some("1\n"), Some("2\n"));
        turn.record("a.txt", Some("1\n"), Some("2\n"));
        let diff = turn.diff().unwrap();
        assert!(diff.find("a/a.txt").unwrap() < diff.find("a/b.txt").unwrap());
    }

    #[test]
    fn an_untracked_edit_ends_the_diff() {
        let mut turn = TurnDiff::new();
        turn.record("a.txt", Some("a\n"), Some("b\n"));
        turn.invalidate();
        assert_eq!(turn.diff(), None);
        turn.record("b.txt", Some("a\n"), Some("b\n"));
        assert_eq!(turn.diff(), None);
    }
}
