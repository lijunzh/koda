//! Session picker dropdown — thin wrapper around the generic dropdown.
//!
//! Appears when the user types `/resume` with no args. Typing in it
//! searches, as in Claude Code's session picker: the sessions it opened with
//! are narrowed at once, and the server is asked which sessions mention the
//! text (`session/search`); those follow, each with the words around the
//! match ([`SessionPicker`]).

use super::dropdown::{DropdownItem, DropdownState};

/// A query shorter than this only narrows the sessions the picker has.
pub const MIN_SEARCH: usize = 2;

/// How much of a snippet a row shows before its match, and in all: the id
/// and the name come first, and the match should still fit an 80-column row.
const SNIPPET_LEAD: usize = 12;
const SNIPPET_SHOWN: usize = 60;

/// A session item for the dropdown: its title, or first prompt, and when it
/// was last used, as Codex's session picker shows them.
#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub short_id: String,
    /// When its last message was stored, formatted.
    pub updated: String,
    pub is_current: bool,
    pub title: Option<String>,
    /// The first prompt, for a session without a title.
    pub preview: String,
    /// For a session found by what was said in it: the words around the
    /// match, shown in place of the date.
    pub snippet: Option<String>,
}

impl DropdownItem for SessionItem {
    fn label(&self) -> &str {
        &self.short_id
    }
    fn description(&self) -> String {
        let name = self.title.as_deref().unwrap_or(&self.preview);
        let name: String = name.chars().take(30).collect();
        let second = self.snippet.as_deref().unwrap_or(&self.updated);
        let mut desc = format!("{name}  {second}");
        if self.is_current {
            desc.push_str(" \u{25c0} current");
        }
        desc
    }
    fn matches_filter(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        self.id.to_lowercase().contains(&f)
            || self.updated.to_lowercase().contains(&f)
            || self.preview.to_lowercase().contains(&f)
            || self
                .title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&f))
    }
}

/// `snippet`, as the server cut it around a match of `term`, cut again for
/// a picker row: at most `SNIPPET_LEAD` characters before the match and
/// `SNIPPET_SHOWN` in all, `…` at an end this cut. Letters match in either
/// case; a term not found as typed (the server joins runs of spaces) leaves
/// the start of the snippet.
pub fn row_snippet(snippet: &str, term: &str) -> String {
    let chars: Vec<char> = snippet.chars().collect();
    let lower = |c: &char| c.to_lowercase().next().unwrap_or(*c);
    let wanted: Vec<char> = term.chars().map(|c| lower(&c)).collect();
    let at = chars
        .windows(wanted.len().max(1))
        .position(|window| window.iter().map(lower).eq(wanted.iter().copied()))
        .unwrap_or(0);
    let start = at.saturating_sub(SNIPPET_LEAD);
    let end = chars.len().min(start + SNIPPET_SHOWN);
    let mut row = String::new();
    if start > 0 {
        row.push('\u{2026}');
    }
    row.extend(&chars[start..end]);
    if end < chars.len() {
        row.push('\u{2026}');
    }
    row
}

/// The `/resume` picker: the project's sessions it opened with, what has
/// been typed, and the sessions the server found for that.
pub struct SessionPicker {
    pub dropdown: DropdownState<SessionItem>,
    /// What has been typed.
    query: String,
    /// The dropdown's title before anything is typed.
    title: &'static str,
    /// The sessions the picker opened with.
    loaded: Vec<SessionItem>,
    /// The sessions the server found for `query`, once it has answered.
    found: Vec<SessionItem>,
    /// Where the search for `query` stands.
    search: Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Search {
    /// The query is too short for one.
    None,
    Waiting,
    Answered,
    Failed,
}

impl SessionPicker {
    /// A picker over `loaded`, the current session selected.
    pub fn new(loaded: Vec<SessionItem>, title: &'static str) -> Self {
        let mut picker = Self {
            dropdown: DropdownState::new(Vec::new(), title),
            query: String::new(),
            title,
            loaded,
            found: Vec::new(),
            search: Search::None,
        };
        picker.rebuild();
        if let Some(current) = picker.dropdown.filtered.iter().position(|s| s.is_current) {
            picker.dropdown.select(current);
        }
        picker
    }

    /// What has been typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The query is now `query`: the sessions the picker has are narrowed
    /// to it, and what the server found for the last one goes. The text to
    /// ask the server for, when it is long enough to.
    pub fn set_query(&mut self, query: String) -> Option<String> {
        self.query = query;
        self.found.clear();
        let term = search_term(&self.query);
        self.search = match term {
            Some(_) => Search::Waiting,
            None => Search::None,
        };
        self.rebuild();
        term
    }

    /// The server's answer for `term`: the sessions that mention it, or
    /// that the search failed. An answer for a query typed past is dropped;
    /// whether this one was kept.
    pub fn answered(&mut self, term: &str, found: Result<Vec<SessionItem>, String>) -> bool {
        if search_term(&self.query).as_deref() != Some(term) {
            return false;
        }
        let selected = self.dropdown.selected_item().map(|item| item.id.clone());
        match found {
            Ok(found) => {
                self.found = found;
                self.search = Search::Answered;
            }
            Err(_) => self.search = Search::Failed,
        }
        self.rebuild();
        // The row the user was on stays theirs when the answer adds rows.
        if let Some(at) =
            selected.and_then(|id| self.dropdown.filtered.iter().position(|item| item.id == id))
        {
            self.dropdown.select(at);
        }
        true
    }

    /// The rows for the query: the picker's own sessions that match it,
    /// then the sessions found by what was said in them that aren't among
    /// those.
    fn rebuild(&mut self) {
        let mut items: Vec<SessionItem> = self
            .loaded
            .iter()
            .filter(|item| self.query.trim().is_empty() || item.matches_filter(self.query.trim()))
            .cloned()
            .collect();
        let by_content: Vec<SessionItem> = self
            .found
            .iter()
            .filter(|found| items.iter().all(|item| item.id != found.id))
            .cloned()
            .collect();
        items.extend(by_content);
        let title = if self.query.is_empty() {
            self.title.to_string()
        } else {
            let state = match self.search {
                Search::Waiting => " \u{00b7} searching\u{2026}",
                Search::Failed => " \u{00b7} search failed",
                Search::None | Search::Answered if items.is_empty() => ": none",
                Search::None | Search::Answered => "",
            };
            format!("{} matching \"{}\"{state}", self.title, self.query)
        };
        self.dropdown = DropdownState::new(items, title)
            .with_select_hint("enter select \u{00b7} type to search");
    }
}

/// What to ask the server for when the picker holds `query`: nothing while
/// it is too short.
fn search_term(query: &str) -> Option<String> {
    let term = query.trim();
    (term.chars().count() >= MIN_SEARCH).then(|| term.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: Option<&str>, current: bool) -> SessionItem {
        SessionItem {
            id: "abc12345".into(),
            short_id: "abc12345".into(),
            updated: "2026-03-08 14:30".into(),
            is_current: current,
            title: title.map(str::to_string),
            preview: "fix the login bug".into(),
            snippet: None,
        }
    }

    fn session(id: &str, title: &str) -> SessionItem {
        SessionItem {
            id: id.into(),
            short_id: id.into(),
            title: Some(title.into()),
            ..item(None, false)
        }
    }

    fn found(id: &str, title: &str, snippet: &str) -> SessionItem {
        SessionItem {
            snippet: Some(snippet.into()),
            ..session(id, title)
        }
    }

    fn ids(picker: &SessionPicker) -> Vec<&str> {
        picker
            .dropdown
            .filtered
            .iter()
            .map(|item| item.id.as_str())
            .collect()
    }

    #[test]
    fn current_shows_marker() {
        assert!(item(None, true).description().contains('\u{25c0}'));
    }

    #[test]
    fn a_session_without_a_title_shows_its_first_prompt() {
        let desc = item(None, false).description();
        assert!(desc.starts_with("fix the login bug"), "{desc}");
        assert!(desc.contains("2026-03-08 14:30"), "{desc}");
    }

    #[test]
    fn filter_by_id_date_title_and_prompt() {
        let item = item(Some("refactor auth module"), false);
        assert!(item.matches_filter("abc"));
        assert!(item.matches_filter("2026"));
        assert!(item.matches_filter("refactor"));
        assert!(item.matches_filter("login"));
        assert!(!item.matches_filter("xyz"));
    }

    /// A session found by its content shows the words around the match
    /// where the others show the date.
    #[test]
    fn a_content_result_shows_its_snippet_for_the_date() {
        let desc = found("a", "Fix the retries", "so the retry budget").description();
        assert_eq!(desc, "Fix the retries  so the retry budget");
    }

    /// Typing narrows the picker's own sessions at once and asks for a
    /// search from the second character; the answer's sessions follow the
    /// ones that match by name, without the ones already there.
    #[test]
    fn typing_narrows_the_sessions_and_the_answer_adds_what_mentions_it() {
        let loaded = vec![session("a", "Retry budget"), session("b", "Parser work")];
        let mut picker = SessionPicker::new(loaded, "Sessions");
        assert_eq!(ids(&picker), ["a", "b"]);
        assert_eq!(picker.dropdown.title, "Sessions");

        assert_eq!(picker.set_query("r".into()), None, "one character");
        assert_eq!(ids(&picker), ["a", "b"], "both names have an r");
        assert_eq!(picker.set_query("retry".into()).as_deref(), Some("retry"));
        assert_eq!(ids(&picker), ["a"]);
        assert!(
            picker.dropdown.title.contains("searching"),
            "{}",
            picker.dropdown.title
        );

        let answer = vec![
            found("a", "Retry budget", "the retry here"),
            found("b", "Parser work", "then retry the parse"),
            found("c", "Old session", "a retry loop"),
        ];
        assert!(picker.answered("retry", Ok(answer)));
        assert_eq!(ids(&picker), ["a", "b", "c"]);
        let rows = &picker.dropdown.filtered;
        assert!(rows[0].snippet.is_none(), "matched by name: as listed");
        assert_eq!(rows[1].snippet.as_deref(), Some("then retry the parse"));
        assert_eq!(picker.dropdown.title, "Sessions matching \"retry\"");
    }

    /// The server answers a query the user has typed past: nothing of it
    /// shows, and the picker still waits for its own.
    #[test]
    fn an_answer_for_an_older_query_is_dropped() {
        let mut picker = SessionPicker::new(vec![session("a", "Parser work")], "Sessions");
        picker.set_query("ze".into());
        picker.set_query("zebra".into());
        assert!(!picker.answered("ze", Ok(vec![found("old", "Zen", "ze")])));
        assert!(ids(&picker).is_empty());
        assert!(picker.dropdown.title.ends_with("searching\u{2026}"));
        assert!(picker.answered("zebra", Ok(vec![found("c", "Crossing", "the zebra")])));
        assert_eq!(ids(&picker), ["c"]);
        // Back to a query too short to search: the last answer is gone.
        picker.set_query("z".into());
        assert!(!picker.answered("zebra", Ok(vec![found("c", "Crossing", "the zebra")])));
        assert!(ids(&picker).is_empty());
        assert_eq!(picker.dropdown.title, "Sessions matching \"z\": none");
    }

    /// The answer adds rows under the selection without moving it.
    #[test]
    fn the_answer_keeps_the_selected_row() {
        let loaded = vec![session("a", "Retry one"), session("b", "Retry two")];
        let mut picker = SessionPicker::new(loaded, "Sessions");
        picker.set_query("retry".into());
        picker.dropdown.down();
        picker.answered("retry", Ok(vec![found("c", "Other", "a retry")]));
        assert_eq!(picker.dropdown.selected_item().unwrap().id, "b");
    }

    /// A failed search says so and keeps the sessions matched by name.
    #[test]
    fn a_failed_search_keeps_the_name_matches() {
        let mut picker = SessionPicker::new(vec![session("a", "Retry one")], "Sessions");
        picker.set_query("retry".into());
        assert!(picker.answered("retry", Err("server gone".into())));
        assert_eq!(ids(&picker), ["a"]);
        assert!(picker.dropdown.title.ends_with("search failed"));
    }

    /// A row's snippet starts near its match, so the match shows in a row
    /// that also has the id and the name.
    #[test]
    fn a_row_snippet_is_cut_around_the_match() {
        let server = "... a long lead of words before the place where the Zebra crossing is, and then a good deal more text after it that a row has no room for ...";
        let cases = [
            ("the zebra crossing", "zebra", "the zebra crossing"),
            ("no such word here", "giraffe", "no such word here"),
            (
                server,
                "zebra",
                "\u{2026}e where the Zebra crossing is, and then a good deal more tex\u{2026}",
            ),
        ];
        for (snippet, term, row) in cases {
            assert_eq!(row_snippet(snippet, term), row, "{snippet:?}");
        }
    }
}
