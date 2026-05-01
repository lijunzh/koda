//! Composer surface: textarea, paste-burst handling, text elements.
//!
//! This module is the in-progress hybrid port of codex's composer
//! primitives (#1116, #1178). The textarea + paste-burst sub-modules
//! are lifted from codex (Apache 2.0) with minor adaptation to koda's
//! crossterm/ratatui versions; the small support types (text_element,
//! key_hint) are reduced ports keeping only what the textarea needs.
//!
//! ## Status
//!
//! - **PR 1 (this PR — #1178):** ports the primitives. No behavioral
//!   change: koda still uses `ratatui-textarea` everywhere; new types
//!   live alongside as tested-but-unused code.
//! - **PR 2 (next):** swaps `ratatui-textarea` for [`textarea::TextArea`]
//!   in the composer surface and drops the dep.
//! - **PR 3 (after PR 2):** adds slash popup + history navigation
//!   widgets that orbit the new textarea.
//!
//! See #1116 for the full plan.

pub mod key_hint;
pub mod keymap;
pub mod paste_burst;
pub mod text_element;
pub mod textarea;
pub mod wrapping;
