//! koda-ast: tree-sitter intelligence for the koda ecosystem.
//!
//! This crate owns everything that derives information from a tree-sitter
//! parse tree:
//!
//! - [`analysis`] — file structure summaries, call graphs, post-edit
//!   syntax checking.
//! - [`highlight`] — language-agnostic semantic-token highlighting,
//!   with a tree-sitter primary backend (Phase 2) and a syntect
//!   fallback for unsupported languages.
//! - [`grammar`] — single-source-of-truth language registry shared by
//!   both subsystems.
//! - [`tokens`] — the [`SemanticToken`] enum that's the lingua
//!   franca for renderers.
//!
//! Pure library — no binary, no MCP server, no async runtime. Library
//! consumers (`koda-cli`, future LSP backend, web playground) embed
//! this crate directly via `cargo`.

pub mod analysis;
pub mod grammar;
pub mod highlight;
pub mod tokens;

/// Re-export core analysis functions at the crate root for convenience.
pub use analysis::{analyze_file, get_call_graph, syntax_check};
/// Re-export highlight types for convenient `use koda_ast::*;` style.
pub use highlight::{LanguageHint, highlight_spans};
pub use tokens::{HighlightSpan, SemanticToken};
