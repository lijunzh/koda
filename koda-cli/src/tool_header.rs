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
pub fn detail_spans(name: &str, args: &Value) -> Vec<Span<'static>> {
    match name {
        "Bash" => bash_detail(args),
        "Read" | "Write" | "Edit" | "Delete" => path_detail(args),
        "Grep" => grep_detail(args),
        "Glob" => glob_detail(args),
        "List" => list_detail(args),
        "WebFetch" => webfetch_detail(args),
        _ => generic_detail(args),
    }
}

/// Plain-text rendering of a tool call detail (no styling).
///
/// Mirrors [`detail_spans`] but returns a `String` for non-TUI
/// consumers (clipboard / file export via `transcript.rs`). Per-tool
/// dispatch is kept identical so the same `(name, args)` pair always
/// produces the same human-readable summary regardless of output medium.
///
/// `bash_chars` controls Bash command truncation (typically 80 in
/// summary mode, 500 in verbose). All other tools use
/// [`MAX_DETAIL_CHARS`] for consistency with TUI rendering.
pub fn detail_text(name: &str, args: &Value, bash_chars: usize) -> String {
    match name {
        "Bash" => bash_text(args, bash_chars),
        "Read" | "Write" | "Edit" | "Delete" => path_text(args),
        "Grep" => grep_text(args),
        "Glob" => glob_text(args),
        "List" => list_text(args),
        "WebFetch" => webfetch_text(args),
        _ => generic_text(args),
    }
}

// ── Per-tool detail builders ──────────────────────────────────────────

fn bash_detail(args: &Value) -> Vec<Span<'static>> {
    let cmd = first_string(args, &["command", "cmd"]).unwrap_or_default();
    if cmd.is_empty() {
        return Vec::new();
    }
    let truncated = truncate_for_header(&cmd);
    highlight::highlight_inline(&truncated, "bash")
}

fn path_detail(args: &Value) -> Vec<Span<'static>> {
    let path = first_string(args, &["file_path", "path"]).unwrap_or_default();
    if path.is_empty() {
        return Vec::new();
    }
    vec![Span::styled(truncate_for_header(&path), theme::PATH)]
}

fn grep_detail(args: &Value) -> Vec<Span<'static>> {
    let pattern = first_string(args, &["search_string", "pattern"]).unwrap_or_default();
    if pattern.is_empty() {
        return Vec::new();
    }
    // Key order matches `tools::grep::run`'s arg lookup so the rendered
    // detail always shows the path the tool actually searched. Pre-fix
    // we checked obsolete `directory`/`path` keys that never matched
    // the schema (the param has been `file_path` since day one), so
    // every Grep header silently rendered "in ." regardless of the
    // real target. See koda#1099 for the cross-tool repro.
    let dir =
        first_string(args, &["file_path", "path", "directory"]).unwrap_or_else(|| ".".to_string());
    vec![
        Span::styled(format!("\"{pattern}\""), theme::MATCH_HIT),
        Span::styled(" in ".to_string(), DIM),
        Span::styled(truncate_for_header(&dir), theme::PATH),
    ]
}

fn glob_detail(args: &Value) -> Vec<Span<'static>> {
    let pattern = first_string(args, &["pattern"]).unwrap_or_default();
    if pattern.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::styled(truncate_for_header(&pattern), theme::PATH)];
    // Glob takes an optional `file_path` base directory. When present,
    // surface it so the user can tell `Glob '*.toml'` from project
    // root vs from `koda-cli/`. Hidden when omitted (project root
    // default) to keep headers tight for the common case.
    if let Some(base) = first_string(args, &["file_path", "path", "directory"]) {
        spans.push(Span::styled(" in ".to_string(), DIM));
        spans.push(Span::styled(truncate_for_header(&base), theme::PATH));
    }
    spans
}

fn list_detail(args: &Value) -> Vec<Span<'static>> {
    // Same key ordering as `tools::file_tools::list_directory` — if
    // the dispatch reads `file_path` first, the renderer must too,
    // or the header lies about which directory was actually listed.
    let dir =
        first_string(args, &["file_path", "path", "directory"]).unwrap_or_else(|| ".".to_string());
    vec![Span::styled(truncate_for_header(&dir), theme::PATH)]
}

fn webfetch_detail(args: &Value) -> Vec<Span<'static>> {
    let url = first_string(args, &["url"]).unwrap_or_default();
    if url.is_empty() {
        return Vec::new();
    }
    vec![Span::styled(truncate_for_header(&url), theme::PATH)]
}

fn generic_detail(args: &Value) -> Vec<Span<'static>> {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    for (_, v) in obj.iter().take(1) {
        if let Some(s) = v.as_str() {
            return vec![Span::styled(truncate_for_header(s), DIM)];
        }
    }
    Vec::new()
}

// ── Per-tool plain-text builders (no styling, for transcripts) ──────────────

fn bash_text(args: &Value, bash_chars: usize) -> String {
    let cmd = first_string(args, &["command", "cmd"]).unwrap_or_default();
    if cmd.chars().count() > bash_chars {
        truncate_chars(&cmd, bash_chars)
    } else {
        cmd
    }
}

fn path_text(args: &Value) -> String {
    first_string(args, &["file_path", "path"]).unwrap_or_default()
}

fn grep_text(args: &Value) -> String {
    let pattern = first_string(args, &["search_string", "pattern"]).unwrap_or_default();
    if pattern.is_empty() {
        return String::new();
    }
    // Mirror `grep_detail`'s key list so transcript exports show the
    // same directory the live header surfaced.
    let dir =
        first_string(args, &["file_path", "path", "directory"]).unwrap_or_else(|| ".".to_string());
    // Quotes match the TUI rendering (see `grep_detail`) so transcript and
    // live view show the same string for the same args.
    format!("\"{pattern}\" in {dir}")
}

fn glob_text(args: &Value) -> String {
    let pattern = first_string(args, &["pattern"]).unwrap_or_default();
    if pattern.is_empty() {
        return String::new();
    }
    match first_string(args, &["file_path", "path", "directory"]) {
        Some(base) => format!("{pattern} in {base}"),
        None => pattern,
    }
}

fn list_text(args: &Value) -> String {
    first_string(args, &["file_path", "path", "directory"]).unwrap_or_else(|| ".".to_string())
}

fn webfetch_text(args: &Value) -> String {
    first_string(args, &["url"]).unwrap_or_default()
}

fn generic_text(args: &Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    for (_, v) in obj.iter().take(1) {
        if let Some(s) = v.as_str() {
            return if s.chars().count() > MAX_DETAIL_CHARS {
                truncate_chars(s, MAX_DETAIL_CHARS)
            } else {
                s.to_string()
            };
        }
    }
    String::new()
}

// ── Internals ─────────────────────────────────────────────────────────

/// Return the first present string value among the candidate keys.
fn first_string(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()))
}

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
    // structural fix (a single `ToolCallSummary` formatter shared by
    // all four layers) is tracked separately.

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
