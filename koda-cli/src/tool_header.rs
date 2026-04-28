//! Tool-call **header** rendering — `● ToolName <styled detail>`.
//!
//! Sibling to [`crate::tool_output_lines`] which handles tool output
//! *bodies*. This module owns the single-row banner that announces each
//! tool call: the dot color, the tool name (bold), and a styled detail
//! string built from the tool's argument JSON.
//!
//! ## Why a dedicated module
//!
//! Pre-#951 we had two near-identical implementations of this logic —
//! `tui_render::tool_call_styles` (live render) and
//! `history_render::tool_detail_summary` + `render_tool_call_headers`
//! (history replay). They drifted: live used `pattern` for Grep,
//! history used `search_string`; live emitted a single dim string,
//! history did the same; neither colored the file path or
//! syntax-highlighted bash commands. Centralizing here ensures both
//! paths render the same `(name, args)` to the same spans.
//!
//! ## Per-tool detail styling
//!
//! | Tool                       | Detail format                                       |
//! |----------------------------|-----------------------------------------------------|
//! | `Bash`                     | `cmd` syntax-highlighted as bash (single-line)      |
//! | `Read`/`Write`/`Edit`/`Delete` | `file_path` in [`theme::PATH`] (cyan)            |
//! | `Grep`                     | `"<pattern>"` in [`theme::MATCH_HIT`] + ` in ` + `file_path` |
//! | `Glob`                     | `pattern` in [`theme::PATH`] (+ ` in ` + `file_path` if set)  |
//! | `List`                     | `file_path` in [`theme::PATH`]                      |
//! | `WebFetch`                 | `url` in [`theme::PATH`]                            |
//! | _other_                    | first string arg, dim                               |

use crate::highlight;
use crate::theme::{self, BOLD, DIM};
use koda_core::tools::summary::{ToolCallKind, ToolCallSummary};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// Maximum visible bytes for inline command/detail text in TUI headers.
///
/// Used by [`truncate_for_header`] which slices on byte boundaries
/// (cheap, ASCII-correct). Matched in value (not unit) by
/// [`MAX_DETAIL_CHARS`] which serves the char-based path used by
/// transcript export. The two constants share a number deliberately
/// — same visible cap from both rendering paths — but each name
/// matches the unit its caller actually uses.
const MAX_DETAIL_BYTES: usize = 120;

/// Maximum visible chars for transcript-export detail text.
///
/// Used by [`generic_text`] which truncates via [`truncate_chars`]
/// (Unicode-correct, slower). Co-equal to [`MAX_DETAIL_BYTES`] in
/// value so a single tool-arg renders identically in TUI and
/// transcript exports for the common ASCII case.
const MAX_DETAIL_CHARS: usize = 120;

/// Build a complete tool-call header line: dot + name + detail spans.
///
/// `indent` is prepended raw (used by sub-agent rendering).
pub fn build_header_line(indent: &str, name: &str, args: &Value) -> Line<'static> {
    let dot_style = theme::tool_dot(name);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }
    spans.push(Span::styled("\u{25cf} ", dot_style));
    spans.push(Span::styled(name.to_string(), BOLD));
    let detail = detail_spans(name, args);
    if !detail.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(detail);
    }
    Line::from(spans)
}

/// Same as [`build_header_line`] but accepts the JSON args as a string
/// (for the history-replay path which stores OpenAI-style call payloads).
///
/// Invalid JSON degrades to an empty detail rather than panicking.
pub fn build_header_line_from_str(indent: &str, name: &str, args_json: &str) -> Line<'static> {
    let parsed: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
    build_header_line(indent, name, &parsed)
}

/// Build only the styled detail spans for a tool call (no dot/name).
///
/// Returns an empty vec if the args don't carry a useful detail
/// (e.g. unknown tool with no string args).
///
/// Implementation: parses args once into a [`ToolCallSummary`] (which
/// owns *all* arg-key conventions, see `koda_core::tools::summary`)
/// and renders the styled spans by pattern-matching on the resulting
/// [`ToolCallKind`]. Pre-#1100 each tool had its own inline
/// `first_string(args, &[...])` lookup duplicated between here and
/// [`detail_text`]; the duplication caused two drift bugs in two days
/// (#1094 then #1099) before consolidation.
pub fn detail_spans(name: &str, args: &Value) -> Vec<Span<'static>> {
    summary_spans(&ToolCallSummary::from_call(name, args))
}

/// Plain-text rendering of a tool call detail (no styling).
///
/// Mirrors [`detail_spans`] but returns a `String` for non-TUI
/// consumers (clipboard / file export via `transcript.rs`). Both
/// formatters consume the same [`ToolCallSummary`], so the rendered
/// string is guaranteed to describe the same `(tool, args)` pair as
/// the live header.
///
/// `bash_chars` controls Bash command truncation (typically 80 in
/// summary mode, 500 in verbose). All other tools use
/// [`MAX_DETAIL_CHARS`] for consistency with TUI rendering.
pub fn detail_text(name: &str, args: &Value, bash_chars: usize) -> String {
    summary_text(&ToolCallSummary::from_call(name, args), bash_chars)
}

// ── Renderers ─────────────────────────────────────────────────────────
//
// Both renderers pattern-match on the SAME [`ToolCallKind`] so they
// can't drift on which arg key means "the path" or "the query." If a
// tool's display shape changes, both surfaces follow from one edit
// here. If a new tool needs a specialized shape, add a variant in
// `koda_core::tools::summary` and pattern-match it in BOTH renderers
// below — the compiler will fail the missing arm and tell you about it.

/// Render a [`ToolCallSummary`] as styled TUI spans.
fn summary_spans(s: &ToolCallSummary) -> Vec<Span<'static>> {
    match &s.kind {
        ToolCallKind::Bash { command } => {
            if command.is_empty() {
                Vec::new()
            } else {
                highlight::highlight_inline(&truncate_for_header(command), "bash")
            }
        }
        ToolCallKind::Path { path } => {
            if path.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(truncate_for_header(path), theme::PATH)]
            }
        }
        ToolCallKind::Grep { pattern, dir } => {
            if pattern.is_empty() {
                Vec::new()
            } else {
                vec![
                    Span::styled(format!("\"{pattern}\""), theme::MATCH_HIT),
                    Span::styled(" in ".to_string(), DIM),
                    Span::styled(truncate_for_header(dir), theme::PATH),
                ]
            }
        }
        ToolCallKind::Glob { pattern, base } => {
            if pattern.is_empty() {
                Vec::new()
            } else {
                let mut spans = vec![Span::styled(truncate_for_header(pattern), theme::PATH)];
                if let Some(base) = base {
                    spans.push(Span::styled(" in ".to_string(), DIM));
                    spans.push(Span::styled(truncate_for_header(base), theme::PATH));
                }
                spans
            }
        }
        ToolCallKind::List { dir } => {
            // `dir` is never empty — `ToolCallSummary::from_call` defaults
            // to "." when args omit a path. So no emptiness branch needed.
            vec![Span::styled(truncate_for_header(dir), theme::PATH)]
        }
        ToolCallKind::WebFetch { url } => {
            if url.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(truncate_for_header(url), theme::PATH)]
            }
        }
        ToolCallKind::Generic { value } => match value {
            Some(v) => vec![Span::styled(truncate_for_header(v), DIM)],
            None => Vec::new(),
        },
    }
}

/// Render a [`ToolCallSummary`] as plain text (transcripts, clipboards).
fn summary_text(s: &ToolCallSummary, bash_chars: usize) -> String {
    match &s.kind {
        ToolCallKind::Bash { command } => {
            if command.chars().count() > bash_chars {
                truncate_chars(command, bash_chars)
            } else {
                command.clone()
            }
        }
        ToolCallKind::Path { path } => path.clone(),
        ToolCallKind::Grep { pattern, dir } => {
            if pattern.is_empty() {
                String::new()
            } else {
                // Quotes match the TUI rendering above so transcript and
                // live view show the same string for the same args.
                format!("\"{pattern}\" in {dir}")
            }
        }
        ToolCallKind::Glob { pattern, base } => {
            if pattern.is_empty() {
                String::new()
            } else {
                match base {
                    Some(b) => format!("{pattern} in {b}"),
                    None => pattern.clone(),
                }
            }
        }
        ToolCallKind::List { dir } => dir.clone(),
        ToolCallKind::WebFetch { url } => url.clone(),
        ToolCallKind::Generic { value } => match value {
            Some(v) if v.chars().count() > MAX_DETAIL_CHARS => truncate_chars(v, MAX_DETAIL_CHARS),
            Some(v) => v.clone(),
            None => String::new(),
        },
    }
}

// ── Internals ─────────────────────────────────────────────────────────

/// Truncate a header detail to `MAX_DETAIL_BYTES` with an ellipsis.
///
/// Operates on **bytes** rather than chars deliberately — koda's tool
/// args are overwhelmingly ASCII (paths, commands, regexes), and a
/// byte-safe slice via `.char_indices()` keeps us correct for the rare
/// multi-byte path without overcomplicating the hot path.
fn truncate_for_header(s: &str) -> String {
    if s.len() <= MAX_DETAIL_BYTES {
        return s.to_string();
    }
    let cut = s
        .char_indices()
        .take_while(|(i, _)| *i < MAX_DETAIL_BYTES - 1)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}\u{2026}", &s[..cut])
}

/// Truncate `s` to at most `max` **chars** (not bytes), appending `…`.
///
/// Char-based (not byte-based) so callers can express limits in terms of
/// what the user actually sees in a transcript / clipboard preview.
/// Multi-byte safe: never splits a codepoint.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}\u{2026}", &s[..idx]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use serde_json::json;

    fn span_texts(spans: &[Span<'static>]) -> Vec<String> {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn span_styles(spans: &[Span<'static>]) -> Vec<Style> {
        spans.iter().map(|s| s.style).collect()
    }

    // ── path_detail ──────────────────────────────────────────────────

    #[test]
    fn read_uses_path_style() {
        let spans = detail_spans("Read", &json!({"file_path": "src/main.rs"}));
        assert_eq!(span_texts(&spans), vec!["src/main.rs"]);
        assert_eq!(spans[0].style, theme::PATH, "path should be PATH-styled");
    }

    #[test]
    fn write_falls_back_to_path_key() {
        let spans = detail_spans("Write", &json!({"path": "x.txt"}));
        assert_eq!(span_texts(&spans), vec!["x.txt"]);
    }

    #[test]
    fn delete_with_no_path_returns_empty() {
        let spans = detail_spans("Delete", &json!({}));
        assert!(spans.is_empty());
    }

    // ── grep_detail ──────────────────────────────────────────────────

    #[test]
    fn grep_emits_quoted_pattern_then_dir() {
        let spans = detail_spans(
            "Grep",
            &json!({"search_string": "TODO", "directory": "src"}),
        );
        let texts = span_texts(&spans);
        assert_eq!(texts, vec!["\"TODO\"", " in ", "src"]);
        let styles = span_styles(&spans);
        assert_eq!(styles[0], theme::MATCH_HIT);
        assert_eq!(styles[1], DIM);
        assert_eq!(styles[2], theme::PATH);
    }

    #[test]
    fn grep_default_directory_is_dot() {
        let spans = detail_spans("Grep", &json!({"pattern": "foo"}));
        assert_eq!(span_texts(&spans), vec!["\"foo\"", " in ", "."]);
    }

    #[test]
    fn grep_accepts_pattern_alias() {
        // Live-render path uses `pattern`; history uses `search_string`.
        // Both should produce identical spans.
        let live = detail_spans("Grep", &json!({"pattern": "x"}));
        let history = detail_spans("Grep", &json!({"search_string": "x"}));
        assert_eq!(span_texts(&live), span_texts(&history));
    }

    /// Regression for koda#1099: the renderer used to look for
    /// `directory`/`path` keys, but every koda built-in tool's schema
    /// uses `file_path` for the directory parameter. Result: every
    /// `Grep "X" in src/` rendered as `Grep "X" in .` because the
    /// renderer never found the actual key, falling back to the
    /// project-root default. The user's tool trace looked like the
    /// model was spinning on `.` when it was actually walking
    /// distinct subdirectories.
    #[test]
    fn grep_uses_file_path_key_from_schema() {
        let spans = detail_spans(
            "Grep",
            &json!({"pattern": "TODO", "file_path": "koda-core/src"}),
        );
        assert_eq!(
            span_texts(&spans),
            vec!["\"TODO\"", " in ", "koda-core/src"],
            "Grep header must surface `file_path` (the schema's actual key); regression for the silent `in .` rendering"
        );
    }

    // ── bash_detail ──────────────────────────────────────────────────

    #[test]
    fn bash_returns_at_least_one_span() {
        let spans = detail_spans("Bash", &json!({"command": "ls -la"}));
        assert!(!spans.is_empty());
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(combined, "ls -la");
    }

    #[test]
    fn bash_with_no_command_returns_empty() {
        let spans = detail_spans("Bash", &json!({}));
        assert!(spans.is_empty());
    }

    #[test]
    fn bash_flattens_multiline_commands() {
        let spans = detail_spans("Bash", &json!({"command": "echo a\necho b"}));
        let combined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !combined.contains('\n'),
            "header must remain single-line, got {combined:?}"
        );
    }

    // ── glob_detail ──────────────────────────────────────────────────

    #[test]
    fn glob_uses_path_style() {
        let spans = detail_spans("Glob", &json!({"pattern": "**/*.rs"}));
        assert_eq!(spans[0].style, theme::PATH);
        assert_eq!(span_texts(&spans), vec!["**/*.rs"]);
    }

    /// koda#1099: Glob's optional base directory (`file_path`) was
    /// silently dropped from the header. `Glob '*/.git'` searched
    /// from project root vs `Glob '*/.git' in koda-core/` looked
    /// identical to the user.
    #[test]
    fn glob_surfaces_file_path_when_present() {
        let spans = detail_spans(
            "Glob",
            &json!({"pattern": "**/*.rs", "file_path": "koda-core"}),
        );
        assert_eq!(
            span_texts(&spans),
            vec!["**/*.rs", " in ", "koda-core"],
            "Glob header must show its base directory when set; users can't otherwise tell scoped vs root searches apart"
        );
    }

    #[test]
    fn glob_omits_base_when_default_root() {
        // No `file_path` => keep the header tight (the common case).
        let spans = detail_spans("Glob", &json!({"pattern": "*.toml"}));
        assert_eq!(span_texts(&spans), vec!["*.toml"]);
    }

    // ── list_detail ─────────────────────────────────────────────

    #[test]
    fn list_default_directory_is_dot() {
        let spans = detail_spans("List", &json!({}));
        assert_eq!(span_texts(&spans), vec!["."]);
    }

    /// koda#1099: the user-facing repro. The model called
    /// `List { file_path: "src" }`, `List { file_path: "tests" }`,
    /// `List { file_path: "docs" }`, ... and the TUI rendered
    /// `● List .` for every single one because the renderer never
    /// looked at `file_path`. The trace looked like a tight loop on
    /// the project root.
    #[test]
    fn list_uses_file_path_key_from_schema() {
        let spans = detail_spans("List", &json!({"file_path": "koda-cli/src"}));
        assert_eq!(
            span_texts(&spans),
            vec!["koda-cli/src"],
            "List header must surface `file_path` (the schema's actual key); regression for the silent `● List .` repetition"
        );
    }

    /// Defensive contract test: each path-bearing tool's renderer key
    /// list must include the same primary key the dispatch reads.
    /// If anyone renames `file_path` in a tool schema (or adds a new
    /// path-bearing tool) without also updating `tool_header.rs`,
    /// the live header silently lies. This pins the contract so
    /// that drift fails the build instead.
    #[test]
    fn path_bearing_tools_render_actual_dispatch_key() {
        // (tool name, the JSON we'd send, what the rendered detail must contain)
        let cases: &[(&str, serde_json::Value, &str)] = &[
            ("List", json!({"file_path": "src"}), "src"),
            (
                "Grep",
                json!({"pattern": "x", "file_path": "tests"}),
                "tests",
            ),
            (
                "Glob",
                json!({"pattern": "*.rs", "file_path": "docs"}),
                "docs",
            ),
            ("Read", json!({"file_path": "main.rs"}), "main.rs"),
            ("Write", json!({"file_path": "out.rs"}), "out.rs"),
            ("Edit", json!({"file_path": "x.rs"}), "x.rs"),
            ("Delete", json!({"file_path": "y.rs"}), "y.rs"),
        ];
        for (name, args, expected_path) in cases {
            let concat: String = detail_spans(name, args)
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert!(
                concat.contains(expected_path),
                "`{name}` header must surface the path the dispatch reads (`file_path`). Got {concat:?}, expected to contain {expected_path:?}"
            );
        }
    }

    // ── webfetch_detail ──────────────────────────────────────────────

    #[test]
    fn webfetch_underlines_url() {
        let spans = detail_spans("WebFetch", &json!({"url": "https://example.com"}));
        assert_eq!(span_texts(&spans), vec!["https://example.com"]);
        assert!(
            spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "webfetch url should be underlined"
        );
    }

    // ── generic_detail ───────────────────────────────────────────────

    #[test]
    fn generic_picks_first_string_value() {
        let spans = detail_spans("UnknownTool", &json!({"thing": "hello"}));
        assert_eq!(span_texts(&spans), vec!["hello"]);
        assert_eq!(spans[0].style, DIM);
    }

    #[test]
    fn generic_with_no_string_args_returns_empty() {
        let spans = detail_spans("UnknownTool", &json!({"count": 42}));
        assert!(spans.is_empty());
    }

    // ── truncation ───────────────────────────────────────────────────

    #[test]
    fn truncate_passthrough_below_cap() {
        assert_eq!(truncate_for_header("short"), "short");
    }

    #[test]
    fn truncate_caps_long_input_with_ellipsis() {
        let long = "x".repeat(MAX_DETAIL_BYTES + 50);
        let out = truncate_for_header(&long);
        assert!(out.ends_with('\u{2026}'));
        // Stays under cap (chars ≤ bytes for ASCII; ellipsis is multi-byte
        // but the byte-slice cuts strictly below MAX_DETAIL_BYTES first).
        assert!(out.chars().count() <= MAX_DETAIL_BYTES);
    }

    // ── live ↔ history equivalence ───────────────────────────────────

    #[test]
    fn from_str_matches_typed_args_path_tools() {
        let v = json!({"file_path": "x.rs"});
        let typed = build_header_line("", "Read", &v);
        let stringly = build_header_line_from_str("", "Read", &v.to_string());
        assert_eq!(span_texts(&typed.spans), span_texts(&stringly.spans));
        assert_eq!(span_styles(&typed.spans), span_styles(&stringly.spans));
    }

    #[test]
    fn from_str_invalid_json_degrades_gracefully() {
        let line = build_header_line_from_str("", "Read", "{not json");
        // Header still has dot + bold name, just no detail.
        let texts = span_texts(&line.spans);
        assert!(texts.iter().any(|t| t == "Read"));
    }

    #[test]
    fn build_header_line_indent_threads_through() {
        let line = build_header_line("  ", "Read", &json!({"file_path": "x.rs"}));
        assert_eq!(line.spans[0].content.as_ref(), "  ");
    }

    #[test]
    fn build_header_line_no_indent_skips_empty_span() {
        let line = build_header_line("", "Read", &json!({"file_path": "x.rs"}));
        // First span must not be an empty raw indent.
        assert_ne!(line.spans[0].content.as_ref(), "");
    }

    // ── detail_text (plain-text mirror of detail_spans) ───────────────────

    #[test]
    fn detail_text_matches_detail_spans_concat() {
        // For every supported tool, the plain text must equal the
        // concatenation of the styled spans — the two functions are
        // documented as mirrors of one another. Drift between them is
        // the bug this test exists to catch.
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("Read", json!({"file_path": "a.rs"})),
            ("Write", json!({"file_path": "b.rs"})),
            ("Edit", json!({"file_path": "c.rs"})),
            ("Delete", json!({"file_path": "d.rs"})),
            ("Grep", json!({"search_string": "TODO", "directory": "src"})),
            ("Glob", json!({"pattern": "**/*.rs"})),
            ("List", json!({"directory": "src"})),
            ("WebFetch", json!({"url": "https://example.com"})),
        ];
        for (name, args) in cases {
            let text = detail_text(name, &args, 500);
            let spans = detail_spans(name, &args);
            let concat: String = spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(
                text, concat,
                "detail_text and detail_spans disagree for {name}"
            );
        }
    }

    #[test]
    fn detail_text_bash_truncates_at_limit() {
        let cmd = "a".repeat(200);
        let args = json!({ "command": cmd });
        let out = detail_text("Bash", &args, 80);
        // Truncated text holds 80 chars + the ellipsis.
        assert_eq!(out.chars().count(), 81, "got {out:?}");
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn detail_text_bash_passes_through_under_limit() {
        let out = detail_text("Bash", &json!({"command": "ls -la"}), 80);
        assert_eq!(out, "ls -la");
    }

    #[test]
    fn detail_text_grep_uses_dot_when_no_directory() {
        let out = detail_text("Grep", &json!({"search_string": "x"}), 80);
        assert_eq!(out, "\"x\" in .");
    }

    #[test]
    fn detail_text_unknown_tool_returns_first_string_arg() {
        let out = detail_text("MysteryTool", &json!({"thing": "hello"}), 80);
        assert_eq!(out, "hello");
    }

    #[test]
    fn detail_text_unknown_tool_with_no_strings_is_empty() {
        let out = detail_text("MysteryTool", &json!({"count": 42}), 80);
        assert_eq!(out, "");
    }

    /// Meta-test pinning the #1100 consolidation: every variant of
    /// `ToolCallKind` must produce non-empty output from BOTH
    /// renderers given a representative non-empty input. If someone
    /// adds a new variant and forgets to handle it in `summary_spans`
    /// or `summary_text`, the compiler's exhaustive-match check
    /// fires *first* (good); but if they handle it with `=> Vec::new()`
    /// or `=> String::new()` to silence the compiler, this test fires
    /// next and tells them they shipped an invisible tool. Keeps both
    /// renderers honest.
    #[test]
    fn every_tool_call_kind_renders_in_both_surfaces() {
        let cases: Vec<(&str, serde_json::Value, &str)> = vec![
            ("Bash", json!({"command": "ls"}), "Bash"),
            ("Read", json!({"file_path": "x"}), "Path"),
            (
                "Grep",
                json!({"search_string": "x", "file_path": "y"}),
                "Grep",
            ),
            ("Glob", json!({"pattern": "*.rs"}), "Glob"),
            ("List", json!({"file_path": "x"}), "List"),
            (
                "WebFetch",
                json!({"url": "https://example.com"}),
                "WebFetch",
            ),
            ("UnknownTool", json!({"k": "v"}), "Generic"),
        ];
        for (name, args, kind_label) in cases {
            let spans = detail_spans(name, &args);
            let text = detail_text(name, &args, 500);
            assert!(
                !spans.is_empty(),
                "summary_spans returned empty for {kind_label} variant ({name}); \
                 either the variant lost its renderer arm or the test input is wrong"
            );
            assert!(
                !text.is_empty(),
                "summary_text returned empty for {kind_label} variant ({name}); \
                 either the variant lost its renderer arm or the test input is wrong"
            );
        }
    }

    // ── cross-layer: dispatch body ↔ TUI header agreement ────────────────
    //
    // koda has multiple independent formatters for tool calls:
    //
    //   1. The dispatch's response body (what the LLM sees)
    //   2. The TUI header (what the human sees live)
    //   3. The transcript export (what the human saves to clipboard)
    //   4. The history replay (what the human sees on session resume)
    //
    // Each was written independently and **none share code**. When the
    // `List` tool's directory parameter standardized on `file_path`,
    // only the dispatch was updated. The body formatter got fixed in
    // PR #1096; the TUI / transcript layers got fixed in PR #1099 —
    // same root symptom ("can't tell which directory was listed")
    // reported twice from two different angles, because no test had
    // pinned the two layers together.
    //
    // This test does. For a real `List` call against a tempdir, the
    // body that ships to the LLM and the header that ships to the
    // user must both surface the same path. If anyone changes the
    // dispatch's arg key without touching `tool_header.rs`, this
    // test fails before the user ever sees a misleading `● List .`.
    //
    // Glob and Grep aren't end-to-end-tested here (they need a
    // `koda_sandbox::fs::FileSystem` impl that koda-cli doesn't depend
    // on); their renderer-vs-dispatch agreement is pinned by
    // `path_bearing_tools_render_actual_dispatch_key` above. The full
    // structural fix landed in #1100: both renderers now consume a
    // single `koda_core::tools::summary::ToolCallSummary` parsed once
    // from the args, so they cannot drift on which key means "the
    // path." `every_tool_call_kind_renders_in_both_surfaces` above
    // pins the consolidation.

    #[tokio::test]
    async fn list_call_path_appears_in_both_body_and_header() {
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        // Distinctive name so substring assertions can't accidentally
        // pass on common tokens like "src" or "tests".
        let sub = "zeppelin";
        fs::create_dir(tmp.path().join(sub)).unwrap();
        fs::write(tmp.path().join(sub).join("hello.txt"), "hi").unwrap();

        let args = json!({ "file_path": sub });

        // Body: what the LLM sees in the tool response.
        let body = koda_core::tools::file_tools::list_files(tmp.path(), &args, 100)
            .await
            .expect("list_files should succeed against a real tempdir");

        // Header: what the user sees on screen.
        let header: String = detail_spans("List", &args)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();

        assert!(
            body.contains(sub),
            "LLM-facing List body must mention the directory it listed; got: {body:?}"
        );
        assert!(
            header.contains(sub),
            "User-facing List header must mention the directory it listed; got: {header:?}. \
             This is the regression class that hit twice (#1094 then #1099) because no \
             cross-layer test pinned the body and the header together."
        );
    }

    #[tokio::test]
    async fn list_distinct_paths_render_distinct_headers() {
        // Direct repro of the user-reported symptom: the model called
        // List on multiple subdirectories and the TUI rendered every
        // call as `● List .`, making distinct exploration look like
        // a tight loop on the project root.
        use std::fs;
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["alpha", "bravo", "charlie"] {
            fs::create_dir(tmp.path().join(name)).unwrap();
        }

        let mut headers = Vec::new();
        let mut bodies = Vec::new();
        for sub in ["alpha", "bravo", "charlie"] {
            let args = json!({ "file_path": sub });
            bodies.push(
                koda_core::tools::file_tools::list_files(tmp.path(), &args, 100)
                    .await
                    .unwrap(),
            );
            let header: String = detail_spans("List", &args)
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            headers.push(header);
        }

        // Each header must equal its subdirectory — no `.` fallback.
        assert_eq!(headers, vec!["alpha", "bravo", "charlie"]);
        // And no two layers can collide on different inputs.
        assert!(
            headers[0] != headers[1] && headers[1] != headers[2] && headers[0] != headers[2],
            "Distinct List calls must produce distinct headers; got {headers:?}"
        );
        // Bodies should each name their own directory too (defense
        // against a future change that drops the body header).
        for (sub, body) in ["alpha", "bravo", "charlie"].iter().zip(&bodies) {
            assert!(
                body.contains(sub),
                "List body for {sub:?} must mention the directory; got {body:?}"
            );
        }
    }
}
