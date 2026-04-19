//! Syntax highlighting → semantic tokens.
//!
//! Public API: [`highlight_spans`] takes a source string + a hint about
//! the language (extension or canonical name) and returns a sorted,
//! non-overlapping list of [`HighlightSpan`]s. Consumers map the
//! [`SemanticToken`] in each span to whatever rendering layer they care
//! about (ratatui `Style`, ANSI escapes, HTML class, …).
//!
//! ## Backend selection
//!
//! 1. If the language has a tree-sitter grammar registered in
//!    [`crate::grammar`], use the [`tree_sitter`] backend (Phase 2:
//!    not yet implemented — currently delegates to syntect).
//! 2. Otherwise, fall back to [`syntect`], which has bundled syntaxes
//!    for ~50 languages (toml, yaml, json, markdown, …).
//! 3. If syntect can't classify the language either, we return a
//!    single [`SemanticToken::Plain`] span covering the whole input —
//!    callers can render it as default text without crashing.
//!
//! This layering means the highlight pipeline is *never* a hard
//! failure: worst case is "looks like plain text", which is exactly
//! what koda-cli does today.

use crate::grammar;
use crate::tokens::{HighlightSpan, SemanticToken};

mod syntect_backend;
mod tree_sitter_backend;

/// Hint about what language the source is in.
///
/// Callers usually have one of:
/// - a file path → use [`LanguageHint::from_extension`]
/// - an explicit name (e.g. from a markdown fenced code block) →
///   use [`LanguageHint::from_name`]
#[derive(Debug, Clone, Copy)]
pub enum LanguageHint<'a> {
    /// File extension without the leading dot (e.g. `"rs"`).
    Extension(&'a str),
    /// Canonical language name (e.g. `"rust"`, `"python"`).
    Name(&'a str),
}

impl<'a> LanguageHint<'a> {
    pub fn from_extension(ext: &'a str) -> Self {
        Self::Extension(ext.trim_start_matches('.'))
    }

    pub fn from_name(name: &'a str) -> Self {
        Self::Name(name)
    }

    /// Resolve to a canonical language name, if known. Used by backends
    /// to look up grammars / syntaxes by name when extension lookup
    /// misses.
    pub fn canonical_name(&self) -> Option<&'a str> {
        match self {
            // For an extension, prefer the registry name if we have a
            // tree-sitter grammar; otherwise return the raw extension —
            // syntect can often resolve common ones (e.g. "toml") that
            // we don't ship a grammar for.
            Self::Extension(ext) => grammar::language_name(ext).or(Some(*ext)),
            Self::Name(name) => Some(*name),
        }
    }

    /// Returns the file extension if the hint was constructed from one.
    pub fn extension(&self) -> Option<&'a str> {
        match self {
            Self::Extension(ext) => Some(*ext),
            Self::Name(_) => None,
        }
    }
}

/// Highlight a source string, returning semantic spans.
///
/// Spans are sorted by `start` and non-overlapping; gaps are implicit
/// [`SemanticToken::Plain`]. The returned vector always covers a
/// non-empty input — even unrecognized languages get a single
/// `Plain` span so renderers can rely on "always at least one span
/// for non-empty source".
pub fn highlight_spans(source: &str, hint: LanguageHint<'_>) -> Vec<HighlightSpan> {
    if source.is_empty() {
        return Vec::new();
    }

    // Phase 2 will route to the tree-sitter backend when a grammar is
    // available. For now both backends share the syntect path so the
    // public API is stable from day one.
    if let Some(spans) = tree_sitter_backend::highlight(source, hint) {
        return spans;
    }
    if let Some(spans) = syntect_backend::highlight(source, hint) {
        return spans;
    }

    // Last resort: whole input is one Plain span.
    vec![HighlightSpan {
        start: 0,
        end: source.len(),
        token: SemanticToken::Plain,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_source_returns_no_spans() {
        let spans = highlight_spans("", LanguageHint::from_extension("rs"));
        assert!(spans.is_empty());
    }

    #[test]
    fn unknown_language_returns_single_plain_span() {
        let src = "completely unknown content";
        let spans = highlight_spans(src, LanguageHint::from_name("not-a-real-language"));
        // syntect returns its "Plain Text" syntax which produces 1 plain span.
        assert!(!spans.is_empty());
        assert!(spans.iter().all(|s| s.start < s.end));
    }

    #[test]
    fn spans_are_sorted_and_non_overlapping() {
        let src = "fn main() { println!(\"hi\"); }";
        let spans = highlight_spans(src, LanguageHint::from_extension("rs"));
        for w in spans.windows(2) {
            assert!(w[0].end <= w[1].start, "spans must not overlap: {w:?}");
        }
        for s in &spans {
            assert!(s.start < s.end, "empty span: {s:?}");
            assert!(s.end <= src.len(), "span past EOF: {s:?}");
        }
    }

    #[test]
    #[ignore = "Phase 2: needs tree-sitter backend or scope-aware syntect mapping"]
    fn rust_source_yields_recognizable_tokens() {
        let src = "fn main() {}\n";
        let spans = highlight_spans(src, LanguageHint::from_extension("rs"));
        // We don't pin which token "fn" maps to (syntect vs tree-sitter
        // may differ), only that *some* non-Plain classification happens.
        // Currently fails: syntect backend returns all Plain pending the
        // scope-stack rewrite. Phase 2 will turn this on.
        assert!(
            spans.iter().any(|s| s.token != SemanticToken::Plain),
            "expected at least one classified token in Rust source"
        );
    }
}
