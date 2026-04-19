//! Tree-sitter highlight backend (Phase 2 — not yet implemented).
//!
//! This module is the *intended* primary highlighter. It will use
//! `tree-sitter-highlight` with vendored or per-grammar `highlights.scm`
//! query files to produce semantic spans grounded in the AST.
//!
//! ## Why a stub?
//!
//! The PR that introduced this module (issue #945, Phase 1) deliberately
//! ships only the public API + crate refactor + syntect fallback. The
//! tree-sitter backend needs:
//!
//! 1. A `highlights.scm` query per supported language. Most upstream
//!    `tree-sitter-*` crates ship one, but we need to load them
//!    consistently. Some require vendoring from the grammar repo.
//! 2. A `Highlighter` lifecycle (it's `!Send`, so we either thread-local
//!    or per-call new — Phase 2 will benchmark and pick).
//! 3. A capture-name → [`SemanticToken`] translator. The skeleton is
//!    already in [`crate::tokens::SemanticToken::from_capture_name`]
//!    so this is a small wiring exercise.
//!
//! Returning `None` from [`highlight`] makes the public
//! [`crate::highlight::highlight_spans`] fall through to syntect — so
//! everything keeps working today, and Phase 2 is a pure additive
//! upgrade with no API changes.

use super::LanguageHint;
use crate::tokens::HighlightSpan;

/// Always returns `None` for now — see module docs.
pub fn highlight(_source: &str, _hint: LanguageHint<'_>) -> Option<Vec<HighlightSpan>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_returns_none_so_caller_falls_through() {
        assert!(highlight("fn x() {}", LanguageHint::from_extension("rs")).is_none());
    }
}
