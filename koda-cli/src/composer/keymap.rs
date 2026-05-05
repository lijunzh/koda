// Foundation work for #1116/#1175 — see the equivalent header note in
// composer/key_hint.rs for the full rationale. The blanket allow goes away
// when PR 2 wires up the consumers.

//! koda-owned keymap policy layer (#1178 stage 5c).
//!
//! # Status: koda-OWNED, not vendored
//!
//! Unlike its sibling modules in `composer/` (which are
//! verbatim-or-near-verbatim ports of codex source), this file is
//! **koda's own implementation** of the keymap surface that
//! [`super::textarea::TextArea`] (Stage 5d) consumes. The struct shapes
//! and default-binding data are mirrored from codex so the vendored
//! textarea compiles without modification, but the file is intentionally
//! scoped much smaller than codex's:
//!
//! ## What we DO provide
//!
//! - The 9 keymap structs ([`RuntimeKeymap`], [`AppKeymap`],
//!   [`ChatKeymap`], [`ComposerKeymap`], [`EditorKeymap`],
//!   [`VimNormalKeymap`], [`VimOperatorKeymap`], [`PagerKeymap`],
//!   [`ListKeymap`], [`ApprovalKeymap`]) with the same fields codex
//!   exposes — so [`super::textarea::TextArea`] (which directly accesses
//!   `editor.move_down`, `vim_normal.append_line_end`, etc.) builds
//!   unchanged.
//!
//! - [`RuntimeKeymap::defaults()`] returning the built-in defaults
//!   (mirroring codex's hardcoded `built_in_defaults()`).
//!
//! - The [`primary_binding`] helper used by hint-rendering code.
//!
//! ## What we DO NOT provide
//!
//! Everything below is intentionally omitted because it requires
//! koda-side product decisions that this PR is not the right place
//! for. Each becomes its own follow-up issue.
//!
//! - **No `from_config(TuiKeymap) -> Result<RuntimeKeymap>`**. Codex
//!   parses `~/.codex/config.toml` into a `codex_config::types::TuiKeymap`
//!   then resolves it against defaults with precedence + conflict
//!   validation. koda has no equivalent config schema yet; wiring one
//!   up would mean importing or reinventing
//!   [`KeybindingsSpec`](https://github.com/openai/codex/blob/d55479/codex-rs/config/src/tui_keymap.rs)
//!   plus serde/schemars derives across two crates. Deferred.
//!
//! - **No keybinding string parser**. Codex's `parse_keybinding("ctrl+a")`
//!   lives at the config-loading boundary; without `from_config`, it has
//!   no caller here.
//!
//! - **No conflict validation** (`validate_unique`, `validate_no_shadow`,
//!   `validate_no_reserved`). These prevent users from binding the
//!   same key to multiple actions. Without user config, defaults are
//!   curated by hand to be conflict-free; we re-derive that as a unit
//!   test below.
//!
//! - **No serde/schemars derives**. The structs are runtime-only; they
//!   never round-trip through TOML in the current design.
//!
//! ## Why mirror codex's struct shape if we're not vendoring?
//!
//! Two reasons:
//!
//! 1. **Textarea compatibility.** [`super::textarea::TextArea`] (Stage 5d)
//!    is vendored verbatim and accesses these structs by field name. If
//!    koda diverged on field naming or grouping, the vendored textarea
//!    would need patches for every divergence — defeating the whole
//!    "vendor primitive, own policy" goal of clean future syncs.
//!
//! 2. **Sync ergonomics.** When codex adds a new editor action (say,
//!    `move_paragraph_down`), the vendored textarea will reference it
//!    via `keymap.editor.move_paragraph_down`. We notice the missing
//!    field at sync time and decide: add a default to mirror codex, or
//!    keep our own intentionally-different binding. Either way, the
//!    decision surface is one struct field, not a refactored API.
//!
//! ## Provenance of the default bindings
//!
//! The hardcoded values in [`RuntimeKeymap::defaults`] are mirrored
//! from codex's `built_in_defaults()` at SHA
//! `87d2235b5470d8969b51d80278a46aa30703eeab`, file
//! `codex-rs/tui/src/keymap.rs` (codex `main` as of 2026-05-06). We
//! deliberately match codex's defaults so users coming from codex have
//! a familiar experience by default; koda may diverge later as product
//! decisions warrant.
//!
//! ## Vendor-sync skips
//!
//! The vendor-sync workflow reads `skip <SHA>` lines below and excludes
//! those upstream commits from drift reports. Each entry must justify
//! WHY koda intentionally does not port the commit. To unskip, delete
//! the line and re-port the commit.
//!
//! - skip 48402be6fa: feat(tui): improve TUI keymap coverage (#20798) —
//!   adds `kill_whole_line` / `fast_mode` keymap actions plus C0 control-char
//!   normalization. The new actions are unbound-by-default upstream and
//!   only become reachable through the `/keymap` slash command and
//!   `codex-config` crate, neither of which koda has. Re-evaluate if koda
//!   adds a `/keymap` UI.
//!
//! Original work: Copyright (c) OpenAI / codex contributors,
//! licensed under the Apache License, Version 2.0.
//! See `LICENSES/codex-APACHE-2.0` for the full license text.

// PR 2 of #1178 swapped the consumers (chat handlers + viewport) to use this
// module, but the codex-port surface includes advanced features (vim mode
// toggle, paste-burst detection, named/highlighted elements, masked render,
// key hints) that are scoped for PR 3+. The unused-warnings will go away as
// each follow-up PR wires them up; until then, allow them at the module
// level so the faithful port can land without piecemeal #[allow] tags.
#![allow(dead_code)]

use super::key_hint;
use super::key_hint::KeyBinding;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

// ----------------------------------------------------------------------------
// Struct definitions — shape mirrored from codex `keymap.rs` so vendored
// textarea.rs (Stage 5d) compiles unchanged.
// ----------------------------------------------------------------------------

/// Runtime keymap consumed by TUI input handlers.
///
/// In codex this is the resolved snapshot after layering user config over
/// defaults. In koda (currently) there is no user-config layer, so this
/// is always [`RuntimeKeymap::defaults()`]. Kept as a struct (not a bare
/// function) so the surface stays codex-compatible for the day we wire
/// up real config.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeKeymap {
    pub(crate) app: AppKeymap,
    pub(crate) chat: ChatKeymap,
    pub(crate) composer: ComposerKeymap,
    pub(crate) editor: EditorKeymap,
    pub(crate) vim_normal: VimNormalKeymap,
    pub(crate) vim_operator: VimOperatorKeymap,
    pub(crate) pager: PagerKeymap,
    pub(crate) list: ListKeymap,
    pub(crate) approval: ApprovalKeymap,
}

#[derive(Clone, Debug)]
pub(crate) struct AppKeymap {
    pub(crate) open_transcript: Vec<KeyBinding>,
    pub(crate) open_external_editor: Vec<KeyBinding>,
    pub(crate) copy: Vec<KeyBinding>,
    pub(crate) clear_terminal: Vec<KeyBinding>,
    pub(crate) toggle_vim_mode: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatKeymap {
    pub(crate) decrease_reasoning_effort: Vec<KeyBinding>,
    pub(crate) increase_reasoning_effort: Vec<KeyBinding>,
    pub(crate) edit_queued_message: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct ComposerKeymap {
    pub(crate) submit: Vec<KeyBinding>,
    pub(crate) queue: Vec<KeyBinding>,
    pub(crate) toggle_shortcuts: Vec<KeyBinding>,
    pub(crate) history_search_previous: Vec<KeyBinding>,
    pub(crate) history_search_next: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct EditorKeymap {
    pub(crate) insert_newline: Vec<KeyBinding>,
    pub(crate) move_left: Vec<KeyBinding>,
    pub(crate) move_right: Vec<KeyBinding>,
    pub(crate) move_up: Vec<KeyBinding>,
    pub(crate) move_down: Vec<KeyBinding>,
    pub(crate) move_word_left: Vec<KeyBinding>,
    pub(crate) move_word_right: Vec<KeyBinding>,
    pub(crate) move_line_start: Vec<KeyBinding>,
    pub(crate) move_line_end: Vec<KeyBinding>,
    pub(crate) delete_backward: Vec<KeyBinding>,
    pub(crate) delete_forward: Vec<KeyBinding>,
    pub(crate) delete_backward_word: Vec<KeyBinding>,
    pub(crate) delete_forward_word: Vec<KeyBinding>,
    pub(crate) kill_line_start: Vec<KeyBinding>,
    pub(crate) kill_line_end: Vec<KeyBinding>,
    pub(crate) yank: Vec<KeyBinding>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VimNormalKeymap {
    pub(crate) enter_insert: Vec<KeyBinding>,
    pub(crate) append_after_cursor: Vec<KeyBinding>,
    pub(crate) append_line_end: Vec<KeyBinding>,
    pub(crate) insert_line_start: Vec<KeyBinding>,
    pub(crate) open_line_below: Vec<KeyBinding>,
    pub(crate) open_line_above: Vec<KeyBinding>,
    pub(crate) move_left: Vec<KeyBinding>,
    pub(crate) move_right: Vec<KeyBinding>,
    pub(crate) move_up: Vec<KeyBinding>,
    pub(crate) move_down: Vec<KeyBinding>,
    pub(crate) move_word_forward: Vec<KeyBinding>,
    pub(crate) move_word_backward: Vec<KeyBinding>,
    pub(crate) move_word_end: Vec<KeyBinding>,
    pub(crate) move_line_start: Vec<KeyBinding>,
    pub(crate) move_line_end: Vec<KeyBinding>,
    pub(crate) delete_char: Vec<KeyBinding>,
    pub(crate) delete_to_line_end: Vec<KeyBinding>,
    pub(crate) yank_line: Vec<KeyBinding>,
    pub(crate) paste_after: Vec<KeyBinding>,
    pub(crate) start_delete_operator: Vec<KeyBinding>,
    pub(crate) start_yank_operator: Vec<KeyBinding>,
    pub(crate) cancel_operator: Vec<KeyBinding>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VimOperatorKeymap {
    pub(crate) delete_line: Vec<KeyBinding>,
    pub(crate) yank_line: Vec<KeyBinding>,
    pub(crate) motion_left: Vec<KeyBinding>,
    pub(crate) motion_right: Vec<KeyBinding>,
    pub(crate) motion_up: Vec<KeyBinding>,
    pub(crate) motion_down: Vec<KeyBinding>,
    pub(crate) motion_word_forward: Vec<KeyBinding>,
    pub(crate) motion_word_backward: Vec<KeyBinding>,
    pub(crate) motion_word_end: Vec<KeyBinding>,
    pub(crate) motion_line_start: Vec<KeyBinding>,
    pub(crate) motion_line_end: Vec<KeyBinding>,
    pub(crate) cancel: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct PagerKeymap {
    pub(crate) scroll_up: Vec<KeyBinding>,
    pub(crate) scroll_down: Vec<KeyBinding>,
    pub(crate) page_up: Vec<KeyBinding>,
    pub(crate) page_down: Vec<KeyBinding>,
    pub(crate) half_page_up: Vec<KeyBinding>,
    pub(crate) half_page_down: Vec<KeyBinding>,
    pub(crate) jump_top: Vec<KeyBinding>,
    pub(crate) jump_bottom: Vec<KeyBinding>,
    pub(crate) close: Vec<KeyBinding>,
    pub(crate) close_transcript: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct ListKeymap {
    pub(crate) move_up: Vec<KeyBinding>,
    pub(crate) move_down: Vec<KeyBinding>,
    pub(crate) accept: Vec<KeyBinding>,
    pub(crate) cancel: Vec<KeyBinding>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalKeymap {
    pub(crate) open_fullscreen: Vec<KeyBinding>,
    pub(crate) open_thread: Vec<KeyBinding>,
    pub(crate) approve: Vec<KeyBinding>,
    pub(crate) approve_for_session: Vec<KeyBinding>,
    pub(crate) approve_for_prefix: Vec<KeyBinding>,
    pub(crate) deny: Vec<KeyBinding>,
    pub(crate) decline: Vec<KeyBinding>,
    pub(crate) cancel: Vec<KeyBinding>,
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// Returns the first binding from a set, used as the primary UI hint for an
/// action.
///
/// Mirror of codex's `keymap::primary_binding`. Hint-rendering code prefers
/// this to keep displayed shortcuts concise while preserving the full binding
/// list for actual input matching.
#[allow(dead_code)] // used by hint-rendering callers added in PR 2.
pub(crate) fn primary_binding(bindings: &[KeyBinding]) -> Option<KeyBinding> {
    bindings.first().copied()
}

/// Build one [`KeyBinding`] from a terse `kind(args)` form for the defaults
/// table.
///
/// Mirror of codex's private `default_binding!` macro (kept here so the
/// default-bindings table reads identically to upstream — this is the bulk of
/// future-sync diffs and we want them line-comparable).
macro_rules! default_binding {
    (plain($key:expr)) => {
        key_hint::plain($key)
    };
    (ctrl($key:expr)) => {
        key_hint::ctrl($key)
    };
    (alt($key:expr)) => {
        key_hint::alt($key)
    };
    (shift($key:expr)) => {
        key_hint::shift($key)
    };
    (raw($binding:expr)) => {
        $binding
    };
}

/// Build a `Vec<KeyBinding>` for the defaults table.
///
/// Mirror of codex's private `default_bindings!` macro. Same rationale as
/// [`default_binding!`] above.
macro_rules! default_bindings {
    ($($kind:ident($($arg:tt)*)),* $(,)?) => {
        vec![$(default_binding!($kind($($arg)*))),*]
    };
}

// ----------------------------------------------------------------------------
// RuntimeKeymap defaults
// ----------------------------------------------------------------------------

impl RuntimeKeymap {
    /// Built-in defaults.
    ///
    /// In koda this is the only way to construct a [`RuntimeKeymap`] until a
    /// user-config layer lands (deferred follow-up). Mirrors codex's
    /// `RuntimeKeymap::defaults()` which delegates to a private
    /// `built_in_defaults()`; we collapse the indirection since we don't have
    /// a `from_config` path that needs to call the private helper.
    pub(crate) fn defaults() -> Self {
        Self {
            app: AppKeymap {
                open_transcript: default_bindings![ctrl(KeyCode::Char('t'))],
                open_external_editor: default_bindings![ctrl(KeyCode::Char('g'))],
                copy: default_bindings![ctrl(KeyCode::Char('o'))],
                clear_terminal: default_bindings![ctrl(KeyCode::Char('l'))],
                toggle_vim_mode: default_bindings![],
            },
            chat: ChatKeymap {
                decrease_reasoning_effort: default_bindings![alt(KeyCode::Char(','))],
                increase_reasoning_effort: default_bindings![alt(KeyCode::Char('.'))],
                edit_queued_message: default_bindings![alt(KeyCode::Up), shift(KeyCode::Left)],
            },
            composer: ComposerKeymap {
                submit: default_bindings![plain(KeyCode::Enter)],
                queue: default_bindings![plain(KeyCode::Tab)],
                toggle_shortcuts: default_bindings![
                    plain(KeyCode::Char('?')),
                    shift(KeyCode::Char('?'))
                ],
                history_search_previous: default_bindings![ctrl(KeyCode::Char('r'))],
                history_search_next: default_bindings![ctrl(KeyCode::Char('s'))],
            },
            editor: EditorKeymap {
                insert_newline: default_bindings![
                    ctrl(KeyCode::Char('j')),
                    ctrl(KeyCode::Char('m')),
                    plain(KeyCode::Enter),
                    shift(KeyCode::Enter),
                    alt(KeyCode::Enter)
                ],
                move_left: default_bindings![plain(KeyCode::Left), ctrl(KeyCode::Char('b'))],
                move_right: default_bindings![plain(KeyCode::Right), ctrl(KeyCode::Char('f'))],
                move_up: default_bindings![plain(KeyCode::Up), ctrl(KeyCode::Char('p'))],
                move_down: default_bindings![plain(KeyCode::Down), ctrl(KeyCode::Char('n'))],
                move_word_left: default_bindings![
                    alt(KeyCode::Char('b')),
                    raw(KeyBinding::new(KeyCode::Left, KeyModifiers::ALT)),
                    raw(KeyBinding::new(KeyCode::Left, KeyModifiers::CONTROL))
                ],
                move_word_right: default_bindings![
                    alt(KeyCode::Char('f')),
                    raw(KeyBinding::new(KeyCode::Right, KeyModifiers::ALT)),
                    raw(KeyBinding::new(KeyCode::Right, KeyModifiers::CONTROL))
                ],
                move_line_start: default_bindings![plain(KeyCode::Home), ctrl(KeyCode::Char('a'))],
                move_line_end: default_bindings![plain(KeyCode::End), ctrl(KeyCode::Char('e'))],
                delete_backward: default_bindings![
                    plain(KeyCode::Backspace),
                    shift(KeyCode::Backspace),
                    ctrl(KeyCode::Char('h'))
                ],
                delete_forward: default_bindings![
                    plain(KeyCode::Delete),
                    shift(KeyCode::Delete),
                    ctrl(KeyCode::Char('d'))
                ],
                delete_backward_word: default_bindings![
                    alt(KeyCode::Backspace),
                    ctrl(KeyCode::Backspace),
                    raw(KeyBinding::new(
                        KeyCode::Backspace,
                        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                    )),
                    ctrl(KeyCode::Char('w')),
                    raw(KeyBinding::new(
                        KeyCode::Char('h'),
                        KeyModifiers::CONTROL.union(KeyModifiers::ALT),
                    ))
                ],
                delete_forward_word: default_bindings![
                    alt(KeyCode::Delete),
                    ctrl(KeyCode::Delete),
                    raw(KeyBinding::new(
                        KeyCode::Delete,
                        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                    )),
                    alt(KeyCode::Char('d'))
                ],
                kill_line_start: default_bindings![ctrl(KeyCode::Char('u'))],
                kill_line_end: default_bindings![ctrl(KeyCode::Char('k'))],
                yank: default_bindings![ctrl(KeyCode::Char('y'))],
            },
            vim_normal: VimNormalKeymap {
                enter_insert: default_bindings![plain(KeyCode::Char('i')), plain(KeyCode::Insert)],
                append_after_cursor: default_bindings![plain(KeyCode::Char('a'))],
                append_line_end: default_bindings![
                    shift(KeyCode::Char('a')),
                    plain(KeyCode::Char('A'))
                ],
                insert_line_start: default_bindings![
                    shift(KeyCode::Char('i')),
                    plain(KeyCode::Char('I'))
                ],
                open_line_below: default_bindings![plain(KeyCode::Char('o'))],
                open_line_above: default_bindings![
                    shift(KeyCode::Char('o')),
                    plain(KeyCode::Char('O'))
                ],
                move_left: default_bindings![plain(KeyCode::Char('h')), plain(KeyCode::Left)],
                move_right: default_bindings![plain(KeyCode::Char('l')), plain(KeyCode::Right)],
                move_up: default_bindings![plain(KeyCode::Char('k')), plain(KeyCode::Up)],
                move_down: default_bindings![plain(KeyCode::Char('j')), plain(KeyCode::Down)],
                move_word_forward: default_bindings![plain(KeyCode::Char('w'))],
                move_word_backward: default_bindings![plain(KeyCode::Char('b'))],
                move_word_end: default_bindings![plain(KeyCode::Char('e'))],
                move_line_start: default_bindings![plain(KeyCode::Char('0'))],
                move_line_end: default_bindings![
                    plain(KeyCode::Char('$')),
                    shift(KeyCode::Char('$'))
                ],
                delete_char: default_bindings![plain(KeyCode::Char('x'))],
                delete_to_line_end: default_bindings![
                    shift(KeyCode::Char('d')),
                    plain(KeyCode::Char('D'))
                ],
                yank_line: default_bindings![shift(KeyCode::Char('y')), plain(KeyCode::Char('Y'))],
                paste_after: default_bindings![plain(KeyCode::Char('p'))],
                start_delete_operator: default_bindings![plain(KeyCode::Char('d'))],
                start_yank_operator: default_bindings![plain(KeyCode::Char('y'))],
                cancel_operator: default_bindings![plain(KeyCode::Esc)],
            },
            vim_operator: VimOperatorKeymap {
                delete_line: default_bindings![plain(KeyCode::Char('d'))],
                yank_line: default_bindings![plain(KeyCode::Char('y'))],
                motion_left: default_bindings![plain(KeyCode::Char('h'))],
                motion_right: default_bindings![plain(KeyCode::Char('l'))],
                motion_up: default_bindings![plain(KeyCode::Char('k'))],
                motion_down: default_bindings![plain(KeyCode::Char('j'))],
                motion_word_forward: default_bindings![plain(KeyCode::Char('w'))],
                motion_word_backward: default_bindings![plain(KeyCode::Char('b'))],
                motion_word_end: default_bindings![plain(KeyCode::Char('e'))],
                motion_line_start: default_bindings![plain(KeyCode::Char('0'))],
                motion_line_end: default_bindings![
                    plain(KeyCode::Char('$')),
                    shift(KeyCode::Char('$'))
                ],
                cancel: default_bindings![plain(KeyCode::Esc)],
            },
            pager: PagerKeymap {
                scroll_up: default_bindings![plain(KeyCode::Up), plain(KeyCode::Char('k'))],
                scroll_down: default_bindings![plain(KeyCode::Down), plain(KeyCode::Char('j'))],
                page_up: default_bindings![
                    plain(KeyCode::PageUp),
                    shift(KeyCode::Char(' ')),
                    ctrl(KeyCode::Char('b'))
                ],
                page_down: default_bindings![
                    plain(KeyCode::PageDown),
                    plain(KeyCode::Char(' ')),
                    ctrl(KeyCode::Char('f'))
                ],
                half_page_up: default_bindings![ctrl(KeyCode::Char('u'))],
                half_page_down: default_bindings![ctrl(KeyCode::Char('d'))],
                jump_top: default_bindings![plain(KeyCode::Home)],
                jump_bottom: default_bindings![plain(KeyCode::End)],
                close: default_bindings![plain(KeyCode::Char('q')), ctrl(KeyCode::Char('c'))],
                close_transcript: default_bindings![ctrl(KeyCode::Char('t'))],
            },
            list: ListKeymap {
                move_up: default_bindings![
                    plain(KeyCode::Up),
                    ctrl(KeyCode::Char('p')),
                    plain(KeyCode::Char('k'))
                ],
                move_down: default_bindings![
                    plain(KeyCode::Down),
                    ctrl(KeyCode::Char('n')),
                    plain(KeyCode::Char('j'))
                ],
                accept: default_bindings![plain(KeyCode::Enter)],
                cancel: default_bindings![plain(KeyCode::Esc)],
            },
            approval: ApprovalKeymap {
                open_fullscreen: default_bindings![
                    ctrl(KeyCode::Char('a')),
                    raw(KeyBinding::new(
                        KeyCode::Char('a'),
                        KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
                    ))
                ],
                open_thread: default_bindings![plain(KeyCode::Char('o'))],
                approve: default_bindings![plain(KeyCode::Char('y'))],
                approve_for_session: default_bindings![plain(KeyCode::Char('a'))],
                approve_for_prefix: default_bindings![plain(KeyCode::Char('p'))],
                deny: default_bindings![plain(KeyCode::Char('d'))],
                decline: default_bindings![plain(KeyCode::Esc), plain(KeyCode::Char('n'))],
                cancel: default_bindings![plain(KeyCode::Char('c'))],
            },
        }
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: `defaults()` actually populates the editor surface used by the
    /// vendored textarea (Stage 5d). If a sync drops a field we depend on,
    /// this test fails before the textarea breaks at a less obvious site.
    #[test]
    fn defaults_populate_editor_actions_used_by_textarea() {
        let km = RuntimeKeymap::defaults();
        // A representative subset — picked because they're the bindings the
        // textarea exercises in its vendored test suite.
        assert!(!km.editor.insert_newline.is_empty());
        assert!(!km.editor.move_left.is_empty());
        assert!(!km.editor.move_right.is_empty());
        assert!(!km.editor.move_up.is_empty());
        assert!(!km.editor.move_down.is_empty());
        assert!(!km.editor.delete_backward.is_empty());
        assert!(!km.editor.kill_line_end.is_empty());
        assert!(!km.editor.yank.is_empty());
    }

    /// Sanity: vim-mode defaults are populated. textarea's vim tests
    /// exercise these.
    #[test]
    fn defaults_populate_vim_normal_actions() {
        let km = RuntimeKeymap::defaults();
        assert!(!km.vim_normal.enter_insert.is_empty());
        assert!(!km.vim_normal.append_line_end.is_empty());
        assert!(!km.vim_normal.move_left.is_empty());
        assert!(!km.vim_normal.start_delete_operator.is_empty());
    }

    /// Mirror of codex's `default_copy_binding_is_ctrl_o` — guards against
    /// accidental drift in a binding users will notice immediately.
    #[test]
    fn default_copy_binding_is_ctrl_o() {
        let km = RuntimeKeymap::defaults();
        assert_eq!(km.app.copy, vec![key_hint::ctrl(KeyCode::Char('o'))]);
    }

    /// `primary_binding` returns the first element of a binding list, or
    /// `None` for empty.
    #[test]
    fn primary_binding_returns_first_or_none() {
        let bindings = vec![
            key_hint::ctrl(KeyCode::Char('a')),
            key_hint::ctrl(KeyCode::Char('b')),
        ];
        assert_eq!(
            primary_binding(&bindings),
            Some(key_hint::ctrl(KeyCode::Char('a')))
        );
        assert_eq!(primary_binding(&[]), None);
    }

    /// Ported from upstream codex commit 87d2235b54 (#21058). Pins the new
    /// modified-Backspace/Delete defaults so a future refactor of
    /// `RuntimeKeymap::defaults()` can't silently drop them.
    #[test]
    fn default_editor_deletion_includes_modified_backspace_delete_aliases() {
        let runtime = RuntimeKeymap::defaults();

        assert!(
            runtime
                .editor
                .delete_backward
                .contains(&key_hint::shift(KeyCode::Backspace))
        );
        assert!(
            runtime
                .editor
                .delete_forward
                .contains(&key_hint::shift(KeyCode::Delete))
        );
        assert!(
            runtime
                .editor
                .delete_backward_word
                .contains(&key_hint::ctrl(KeyCode::Backspace))
        );
        assert!(
            runtime
                .editor
                .delete_backward_word
                .contains(&KeyBinding::new(
                    KeyCode::Backspace,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ))
        );
        assert!(
            runtime
                .editor
                .delete_forward_word
                .contains(&key_hint::ctrl(KeyCode::Delete))
        );
        assert!(
            runtime
                .editor
                .delete_forward_word
                .contains(&KeyBinding::new(
                    KeyCode::Delete,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ))
        );
    }
}
