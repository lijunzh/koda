//! Language registry: `file extension → tree_sitter::Language`.
//!
//! Single source of truth shared by `analysis::*` and `highlight::*`.
//! Anything that needs a tree-sitter grammar should go through here so
//! we add a language in exactly one place.

use anyhow::Result;

/// Map a file extension (no leading dot) to a tree-sitter language.
///
/// Returns an error for unsupported extensions. Callers that want a
/// graceful fallback (e.g. the highlighter) should use
/// [`language_for_extension`] which returns `Option`.
pub fn get_language(extension: &str) -> Result<tree_sitter::Language> {
    language_for_extension(extension).ok_or_else(|| {
        anyhow::anyhow!(
            "Unsupported file type '.{extension}'. Supports: .rs, .py, .pyi, .pyw, \
             .js, .jsx, .mjs, .cjs, .ts, .mts, .cts, .tsx, .go, .java, \
             .c, .h, .cpp, .cc, .cxx, .hpp, .hh, .sh, .bash"
        )
    })
}

/// Non-failing variant of [`get_language`] — returns `None` for unsupported
/// extensions instead of an error. Useful for the highlight pipeline,
/// which falls back to syntect for unsupported languages.
pub fn language_for_extension(extension: &str) -> Option<tree_sitter::Language> {
    Some(match extension {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "py" | "pyi" | "pyw" => tree_sitter_python::LANGUAGE.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "ts" | "mts" | "cts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => tree_sitter_cpp::LANGUAGE.into(),
        "sh" | "bash" => tree_sitter_bash::LANGUAGE.into(),
        _ => return None,
    })
}

/// Stable canonical name for a supported language. Used by the highlight
/// API and by telemetry/diagnostics. Returns `None` for unsupported
/// extensions — callers should fall through to syntect or plain text.
pub fn language_name(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "rust",
        "py" | "pyi" | "pyw" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "sh" | "bash" => "bash",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_resolve() {
        for ext in [
            "rs", "py", "pyi", "js", "jsx", "ts", "tsx", "go", "java", "c", "h", "cpp", "sh",
            "bash",
        ] {
            assert!(
                language_for_extension(ext).is_some(),
                "{ext} should resolve"
            );
            assert!(language_name(ext).is_some(), "{ext} should have a name");
        }
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(language_for_extension("xyz").is_none());
        assert!(language_name("xyz").is_none());
        assert!(get_language("xyz").is_err());
    }
}
