//! `session/search`'s matching: where a term first occurs in a session's
//! text ([`crate::session_text`]), and the text around it.
//!
//! The search is a scan, as Codex's `thread/search` is: the store narrows
//! the sessions to those that may hold the term
//! ([`koda_state::session_store::SessionStore::search_candidates_page`]),
//! and each of those is read here, which decides. One literal piece of
//! text, letters in either case, inside one row; nothing is ranked.

use std::ops::Range;

use koda_protocol::session::SessionSearchMatch;
use koda_state::session_store::search_fold;

use crate::session_text::{Row, RowKind};

/// How much text a snippet keeps around a match, in characters: Codex's
/// (`codex-rs/rollout/src/search.rs`).
const CONTEXT_BEFORE: usize = 48;
const CONTEXT_AFTER: usize = 96;

/// What a search looks for.
pub(crate) struct Term {
    /// The term, each character folded.
    folded: String,
    /// Its length in characters.
    chars: usize,
}

impl Term {
    /// `term`, which isn't empty.
    pub(crate) fn new(term: &str) -> Self {
        Self {
            folded: term.chars().map(search_fold).collect(),
            chars: term.chars().count(),
        }
    }

    /// Where the term first occurs in `text`, in bytes.
    fn find(&self, text: &str) -> Option<Range<usize>> {
        // A character folds to one character, so the match is as many
        // characters into the text as into its folded form.
        let folded: String = text.chars().map(search_fold).collect();
        let before = folded[..folded.find(&self.folded)?].chars().count();
        let mut starts = text.char_indices().map(|(at, _)| at).skip(before);
        let start = starts.next()?;
        let end = starts.nth(self.chars.checked_sub(1)?).unwrap_or(text.len());
        Some(start..end)
    }
}

/// The first place a session says the term.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub turn_id: String,
    pub item_id: String,
    pub matched: SessionSearchMatch,
    pub snippet: String,
}

/// The first of `rows` that has `term`: a tool's output only when
/// `tool_output` says to search those.
pub(crate) fn first_match(rows: &[Row<'_>], term: &Term, tool_output: bool) -> Option<Found> {
    rows.iter()
        .filter(|row| tool_output || row.kind != RowKind::ToolOutput)
        .find_map(|row| {
            let at = term.find(&row.text)?;
            Some(Found {
                turn_id: row.turn_id.to_string(),
                item_id: row.item_id.to_string(),
                matched: match row.kind {
                    RowKind::User => SessionSearchMatch::User,
                    RowKind::Assistant => SessionSearchMatch::Assistant,
                    RowKind::ToolCall => SessionSearchMatch::ToolCall,
                    RowKind::ToolOutput => SessionSearchMatch::ToolOutput,
                },
                snippet: snippet(&row.text, at),
            })
        })
}

/// `text` around the match at `at`, as Codex cuts it: 48 characters before
/// and 96 after, white space run together into single spaces, and `...`
/// at an end that was cut.
fn snippet(text: &str, at: Range<usize>) -> String {
    let start = text[..at.start]
        .char_indices()
        .rev()
        .nth(CONTEXT_BEFORE - 1)
        .map_or(0, |(index, _)| index);
    let end = text[at.end..]
        .char_indices()
        .nth(CONTEXT_AFTER)
        .map_or(text.len(), |(index, _)| at.end + index);
    let excerpt = text[start..end].split_whitespace().collect::<Vec<_>>();
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("... ");
    }
    snippet.push_str(&excerpt.join(" "));
    if end < text.len() {
        snippet.push_str(" ...");
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str, term: &str) -> Option<String> {
        let at = Term::new(term).find(text)?;
        Some(snippet(text, at))
    }

    /// A term is found in either case, as one piece of text, at its place
    /// in the text as written.
    #[test]
    fn a_term_matches_in_either_case() {
        for (text, term, expected) in [
            ("Fix the Parser", "parser", Some("Parser")),
            ("fix the parser", "THE PAR", Some("the par")),
            ("L'ÉCOLE est fermée", "école", Some("ÉCOLE")),
            ("ΟΔΟΣ", "οδοσ", Some("ΟΔΟΣ")),
            // Ⱥ is a byte shorter than its lower case, so the match isn't
            // at the same byte in the folded text.
            ("ȺȺȺ Ⱥ x", "ⱥ x", Some("Ⱥ x")),
            ("fix the parser", "fix parser", None),
            ("a.b", "a_b", None),
            ("", "a", None),
        ] {
            let found = Term::new(term).find(text).map(|at| &text[at]);
            assert_eq!(found, expected, "{term:?} in {text:?}");
        }
    }

    /// The snippet keeps 48 characters before the match and 96 after it,
    /// counted in characters, and says where it cut.
    #[test]
    fn a_snippet_is_cut_around_the_match() {
        let long = |c: char, n: usize| c.to_string().repeat(n);
        let cases = [
            // Short text: all of it.
            ("say needle now".to_string(), "say needle now".to_string()),
            // The match at the start, the text cut after it.
            (
                format!("needle{}", long('a', 200)),
                format!("needle{} ...", long('a', 96)),
            ),
            // The match at the end, the text cut before it.
            (
                format!("{}needle", long('b', 200)),
                format!("... {}needle", long('b', 48)),
            ),
            // Just what fits: nothing was cut.
            (
                format!("{}needle{}", long('b', 48), long('a', 96)),
                format!("{}needle{}", long('b', 48), long('a', 96)),
            ),
            // Characters of several bytes are counted as one each.
            (
                format!("{}needle{}", long('é', 60), long('中', 120)),
                format!("... {}needle{} ...", long('é', 48), long('中', 96)),
            ),
            // White space, line breaks included, is one space; what the cut
            // leaves at an end is dropped.
            (
                format!(
                    "{}\n\nthe  needle\t\tin\n   hay{}",
                    long(' ', 60),
                    long(' ', 120)
                ),
                "... the needle in hay ...".to_string(),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(cut(&text, "NEEDLE"), Some(expected), "{text:?}");
        }
    }

    /// The first row with the term is the match; a tool's output is passed
    /// over unless asked for.
    #[test]
    fn the_first_row_with_the_term_is_the_match() {
        let row = |kind, item_id, text: &'static str| Row {
            kind,
            turn_id: "t1",
            item_id,
            text: text.into(),
        };
        let rows = [
            row(RowKind::User, "u1", "run the tests"),
            row(RowKind::ToolCall, "c1", "Bash\ncommand\tcargo test"),
            row(RowKind::ToolOutput, "c1", "error: the zebra test failed"),
            row(RowKind::Assistant, "m1", "A test named zebra failed."),
        ];
        let found = |term: &str, tool_output| {
            first_match(&rows, &Term::new(term), tool_output)
                .map(|found| (found.item_id, found.matched, found.snippet))
        };
        assert_eq!(
            found("zebra", false),
            Some((
                "m1".to_string(),
                SessionSearchMatch::Assistant,
                "A test named zebra failed.".to_string()
            ))
        );
        assert_eq!(
            found("zebra", true),
            Some((
                "c1".to_string(),
                SessionSearchMatch::ToolOutput,
                "error: the zebra test failed".to_string()
            ))
        );
        assert_eq!(
            found("cargo test", false),
            Some((
                "c1".to_string(),
                SessionSearchMatch::ToolCall,
                "Bash command cargo test".to_string()
            ))
        );
        assert_eq!(found("giraffe", true), None);
    }
}
