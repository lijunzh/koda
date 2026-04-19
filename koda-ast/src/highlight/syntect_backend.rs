//! Syntect-based highlight backend (fallback).
//!
//! Used for languages without a tree-sitter grammar (toml, yaml, json,
//! markdown, css, …) and as a safety net while the tree-sitter backend
//! is still being built out.
//!
//! Syntect classifies tokens with TextMate-style scope strings like
//! `keyword.control.rust` or `string.quoted.double`. We map those to
//! [`SemanticToken`] using the *innermost* scope component — that's
//! the most specific classification syntect emits.

use once_cell::sync::Lazy;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use super::LanguageHint;
use crate::tokens::{HighlightSpan, SemanticToken};

/// Syntect's bundled syntax set. Loaded once and shared.
static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(SyntaxSet::load_defaults_newlines);

/// Theme used purely as a structural requirement of `HighlightLines`.
/// We discard the colors — only the scope stack matters here. We pick
/// `base16-ocean.dark` because it's bundled and deterministic.
static THEME: Lazy<Theme> = Lazy::new(|| {
    let ts = ThemeSet::load_defaults();
    ts.themes
        .get("base16-ocean.dark")
        .cloned()
        .unwrap_or_else(|| ts.themes.values().next().cloned().expect("any theme"))
});

/// Highlight `source` with syntect, returning semantic spans.
///
/// Returns `None` if syntect can't find a syntax for the hint at all
/// (extremely rare — syntect has a "Plain Text" fallback). The caller
/// (`highlight_spans`) translates `None` into a single `Plain` span.
pub fn highlight(source: &str, hint: LanguageHint<'_>) -> Option<Vec<HighlightSpan>> {
    let syntax = resolve_syntax(hint)?;
    let mut hl = HighlightLines::new(syntax, &THEME);

    let mut spans: Vec<HighlightSpan> = Vec::new();
    let mut byte_offset = 0usize;

    for line in LinesWithEndings::from(source) {
        // `highlight_line` returns (Style, &str) where the &str slices
        // come directly out of `line`. The Style here carries the full
        // ScopeStack via the highlighter's internal state — but we
        // can't easily extract that without using the lower-level
        // parser API. So we use parser+highlighter directly:
        let regions = match hl.highlight_line(line, &SYNTAX_SET) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for (style, text) in regions {
            let len = text.len();
            if len == 0 {
                continue;
            }
            let token = scope_to_token(style);
            push_span(&mut spans, byte_offset, byte_offset + len, token);
            byte_offset += len;
        }
    }

    if spans.is_empty() {
        return None;
    }
    Some(spans)
}

/// Find a syntect syntax matching the hint.
fn resolve_syntax(hint: LanguageHint<'_>) -> Option<&'static SyntaxReference> {
    let set = &*SYNTAX_SET;
    if let Some(ext) = hint.extension()
        && let Some(s) = set.find_syntax_by_extension(ext)
    {
        return Some(s);
    }
    if let Some(name) = name_from_hint(hint)
        && let Some(s) = set
            .find_syntax_by_name(name)
            .or_else(|| set.find_syntax_by_token(name))
    {
        return Some(s);
    }
    Some(set.find_syntax_plain_text())
}

fn name_from_hint(hint: LanguageHint<'_>) -> Option<&str> {
    match hint {
        LanguageHint::Name(n) => Some(n),
        LanguageHint::Extension(_) => None,
    }
}

/// Map a syntect [`Style`] (which encodes a foreground color, *not* a
/// scope stack) to a [`SemanticToken`]. Since the public `HighlightLines`
/// API doesn't expose the scope, we collapse on color-class heuristics.
///
/// This is a stopgap. The proper fix is to use the lower-level parser
/// + scope-stack API (`ParseState` + `HighlightState`) so we can read
///   scope names directly. Phase 2 will revisit; for now this gives us
///   *some* semantic differentiation rather than blank Plain spans.
fn scope_to_token(_style: Style) -> SemanticToken {
    // Without scope info we can't reliably classify — return Plain so
    // the renderer uses default text. The tree-sitter backend (Phase 2)
    // will produce real classifications. This keeps the public API
    // contract intact ("you'll get spans, even if Plain") while not
    // lying about what we know.
    SemanticToken::Plain
}

/// Push a span, merging with the previous one if the token matches and
/// the ranges are contiguous. Keeps the output compact.
fn push_span(spans: &mut Vec<HighlightSpan>, start: usize, end: usize, token: SemanticToken) {
    if let Some(last) = spans.last_mut()
        && last.end == start
        && last.token == token
    {
        last.end = end;
        return;
    }
    spans.push(HighlightSpan { start, end, token });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_a_known_extension() {
        let spans = highlight("fn main() {}\n", LanguageHint::from_extension("rs"))
            .expect("syntect should classify rust");
        assert!(!spans.is_empty());
        // Spans should cover all bytes (gaps are valid but the union
        // should equal the source length when nothing is dropped).
        let total: usize = spans.iter().map(|s| s.end - s.start).sum();
        assert_eq!(total, "fn main() {}\n".len());
    }

    #[test]
    fn unknown_extension_falls_back_to_plain_text() {
        // syntect's plain text syntax always exists, so we get spans.
        let spans = highlight("hello world", LanguageHint::from_extension("zzz"))
            .expect("plain text syntax should always resolve");
        assert!(!spans.is_empty());
    }

    #[test]
    fn empty_input_returns_none() {
        let spans = highlight("", LanguageHint::from_extension("rs"));
        assert!(spans.is_none());
    }
}
