//! Language-agnostic semantic tokens for syntax highlighting.
//!
//! `koda-ast` produces `SemanticToken` values; consumers (koda-cli,
//! a future LSP backend, web playground, …) decide how to *render*
//! them. This separation keeps `koda-ast` free of any UI / terminal /
//! color concerns — the SOLID single-responsibility line.
//!
//! The enum is intentionally narrow. We borrow names from the
//! [`tree-sitter-highlight`] standard capture-name set and from the
//! LSP semantic-token spec, choosing the *intersection* — anything
//! both worlds agree on. Backends that emit richer information (e.g.
//! `function.builtin` vs `function.method`) collapse them into the
//! coarsest variant here. Two reasons:
//!
//! 1. Themes only need to define one color per variant.
//! 2. Backend-specific quirks don't leak into the public API.
//!
//! If a real need for more granularity shows up (e.g. theming builtins
//! distinctly), add a variant — don't refactor existing themes.

use serde::{Deserialize, Serialize};

/// A semantic classification for a span of source text.
///
/// Order is stable for serialization; new variants must be appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticToken {
    /// Keywords: `fn`, `if`, `return`, `def`, `class`, …
    Keyword,
    /// Function or method *call site* — e.g. the `bar` in `foo.bar()`.
    /// Distinguished from [`FunctionDef`] so themes can highlight them
    /// differently (one of the wins tree-sitter offers over syntect).
    FunctionCall,
    /// Function or method *definition* — e.g. the `bar` in `fn bar()`.
    FunctionDef,
    /// Type names: structs, classes, interfaces, type aliases.
    Type,
    /// Built-in or primitive types: `i32`, `String`, `bool`, …
    TypeBuiltin,
    /// Identifier in a binding context (variable, parameter, field).
    Variable,
    /// String literals.
    String,
    /// Numeric literals.
    Number,
    /// Boolean and other named constants: `true`, `null`, `None`.
    Constant,
    /// Comments — both line and block.
    Comment,
    /// Operators: `+`, `=`, `=>`, `::`.
    Operator,
    /// Punctuation delimiters: `,`, `;`, brackets, parens.
    Punctuation,
    /// Attributes / decorators: `#[derive(...)]`, `@property`.
    Attribute,
    /// Macro names: `println!`, `vec!`.
    Macro,
    /// Parameters in function signatures (subset of [`Variable`] when
    /// the backend can distinguish them).
    Parameter,
    /// Property / field access: the `bar` in `foo.bar`.
    Property,
    /// Module / namespace identifiers.
    Module,
    /// Anything the backend couldn't classify. Renderers should style
    /// these with the default text color.
    Plain,
}

impl SemanticToken {
    /// Map a [`tree-sitter-highlight`] capture name to a `SemanticToken`.
    ///
    /// The standard capture names are defined by the tree-sitter project
    /// (e.g. `keyword`, `function`, `string.special`). We do prefix
    /// matching so future sub-categories (`keyword.return`, `string.regex`)
    /// still resolve sensibly.
    ///
    /// Returns [`SemanticToken::Plain`] for unknown names — the renderer
    /// will use the default text color, which is the safe fallback.
    pub fn from_capture_name(name: &str) -> Self {
        // Order matters: more specific prefixes first.
        match name {
            n if n.starts_with("comment") => Self::Comment,
            n if n.starts_with("string") => Self::String,
            n if n.starts_with("number") => Self::Number,
            n if n.starts_with("constant.builtin") => Self::Constant,
            n if n.starts_with("constant") => Self::Constant,
            n if n.starts_with("keyword") => Self::Keyword,
            n if n.starts_with("operator") || n == "punctuation.special" => Self::Operator,
            n if n.starts_with("punctuation") => Self::Punctuation,
            n if n.starts_with("function.method.call")
                || n.starts_with("function.call")
                || n == "function.method" =>
            {
                Self::FunctionCall
            }
            n if n.starts_with("function") => Self::FunctionDef,
            n if n.starts_with("type.builtin") => Self::TypeBuiltin,
            n if n.starts_with("type") => Self::Type,
            n if n.starts_with("attribute") => Self::Attribute,
            n if n.starts_with("variable.parameter") => Self::Parameter,
            n if n.starts_with("variable.member") || n.starts_with("property") => Self::Property,
            n if n.starts_with("variable") => Self::Variable,
            n if n.starts_with("module") || n.starts_with("namespace") => Self::Module,
            n if n.ends_with(".macro") || n.starts_with("function.macro") => Self::Macro,
            _ => Self::Plain,
        }
    }
}

/// A classified span of source text.
///
/// Byte offsets refer to the original source string. Spans are
/// non-overlapping and sorted by `start`. Gaps between spans are
/// implicitly [`SemanticToken::Plain`] — the renderer fills them
/// with the default text style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub token: SemanticToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_name_mapping_covers_common_cases() {
        assert_eq!(
            SemanticToken::from_capture_name("keyword"),
            SemanticToken::Keyword
        );
        assert_eq!(
            SemanticToken::from_capture_name("keyword.return"),
            SemanticToken::Keyword
        );
        assert_eq!(
            SemanticToken::from_capture_name("function"),
            SemanticToken::FunctionDef
        );
        assert_eq!(
            SemanticToken::from_capture_name("function.call"),
            SemanticToken::FunctionCall
        );
        assert_eq!(
            SemanticToken::from_capture_name("function.method.call"),
            SemanticToken::FunctionCall
        );
        assert_eq!(
            SemanticToken::from_capture_name("type.builtin"),
            SemanticToken::TypeBuiltin
        );
        assert_eq!(
            SemanticToken::from_capture_name("string.special"),
            SemanticToken::String
        );
        assert_eq!(
            SemanticToken::from_capture_name("comment.line"),
            SemanticToken::Comment
        );
    }

    #[test]
    fn unknown_capture_falls_back_to_plain() {
        assert_eq!(
            SemanticToken::from_capture_name("definitely.not.a.real.name"),
            SemanticToken::Plain
        );
    }

    #[test]
    fn token_serializes_snake_case() {
        // Themes / config files want predictable keys.
        let json = serde_json::to_string(&SemanticToken::FunctionCall).unwrap();
        assert_eq!(json, "\"function_call\"");
    }
}
