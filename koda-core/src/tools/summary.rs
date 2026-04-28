//! Single source of truth for "what does a tool call display?".
//!
//! Pre-#1100 (this module), every rendering surface — TUI live header,
//! transcript export, history replay — re-parsed the raw tool-call
//! JSON args independently. Each surface had its own
//! `first_string(args, &["file_path", "path", "directory"])` lookup
//! list, and they drifted twice in two days:
//!
//! - **#1094 → #1096**: the `List` LLM-facing body lost its
//!   `Listing: <path>` header, leaving the model unable to tell
//!   parallel `List` calls apart.
//! - **#1099**: the TUI header read the wrong arg keys for
//!   `List`/`Grep`/`Glob`, rendering every call as `● List .`
//!   regardless of the actual path. This *hid* the loop-spin bug
//!   #1102 for 8 days because every iteration's `List
//!   /Users/lijun/repo` looked identical to `List .`.
//!
//! Both bugs had the same shape — "a layer that displays a tool call
//! looked at the wrong key in the args JSON" — and required separate
//! fixes in separate files. The root cause was duplication: each
//! rendering layer owned its own JSON parser.
//!
//! ## What this module owns
//!
//! `ToolCallSummary` is a pure data struct that captures everything
//! a renderer needs to know about a tool call:
//!
//! - The tool's name (e.g. `"List"`, `"Grep"`).
//! - The structured payload, in `ToolCallKind` — one variant per
//!   tool family that has its own display shape.
//!
//! `ToolCallSummary::from_call` is the **only** place that knows
//! which arg keys mean "the path" or "the search pattern" for each
//! tool. If a tool's schema renames `file_path` → `target` tomorrow,
//! one constructor changes and every renderer follows.
//!
//! ## What this module does NOT own
//!
//! No rendering knowledge — no `ratatui::Span`, no ANSI colors, no
//! truncation. Renderers (currently `koda-cli/src/tool_header.rs`)
//! pattern-match on the `ToolCallKind` enum and produce their own
//! medium-specific output. This keeps `koda-core` headless and means
//! a future ACP/JSON renderer can plug in without touching this
//! module at all.

use serde_json::Value;

/// A renderer-agnostic description of a tool call's display payload.
///
/// Built once by [`ToolCallSummary::from_call`] from the raw JSON
/// args; consumed by every rendering surface (TUI header, transcript,
/// history replay) via pattern matching on [`Self::kind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallSummary {
    /// Tool name as the dispatcher sees it (e.g. `"List"`, `"Grep"`).
    pub name: String,
    /// Structured per-tool payload.
    pub kind: ToolCallKind,
}

/// Per-tool display payload — one variant per shape, not per tool.
///
/// `Read`/`Write`/`Edit`/`Delete` all share `Path` because they all
/// display "one file path." `Grep` gets its own variant because it
/// has both a pattern and a directory. Variants are added only when
/// a tool's display shape genuinely differs; otherwise we reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallKind {
    /// `Bash` — a single command string.
    Bash {
        /// The shell command to execute. Empty string means the args
        /// didn't carry a recognized command/cmd key.
        command: String,
    },
    /// `Read` / `Write` / `Edit` / `Delete` — a single file path.
    /// Empty string means the args didn't carry a recognized key.
    Path {
        /// The file path the operation targets.
        path: String,
    },
    /// `Grep` — a search pattern in a directory.
    /// `dir` defaults to `"."` when the args omit it.
    Grep {
        /// The search pattern (regex or literal, tool-defined).
        pattern: String,
        /// The directory to search in; `"."` when omitted.
        dir: String,
    },
    /// `Glob` — a glob pattern, optionally rooted at `base`.
    /// `base` is `None` when the call uses the project root default.
    Glob {
        /// The glob pattern (e.g. `"**/*.rs"`).
        pattern: String,
        /// Optional base directory to root the glob at.
        base: Option<String>,
    },
    /// `List` — a directory path. Defaults to `"."` when omitted.
    List {
        /// The directory to list; `"."` when omitted from args.
        dir: String,
    },
    /// `WebFetch` — a URL.
    WebFetch {
        /// The URL to fetch.
        url: String,
    },
    /// Fallback for tools without a specialized shape: the first
    /// string-valued argument (object iteration order), or `None`
    /// if the args have no string values.
    Generic {
        /// First string-valued argument in object-iteration order,
        /// or `None` if no such value exists.
        value: Option<String>,
    },
}

impl ToolCallSummary {
    /// Parse a tool call into its display payload.
    ///
    /// **This is the only function that knows arg-key conventions.**
    /// Adding a new path-bearing tool means adding one match arm
    /// here; renderers pick it up automatically.
    ///
    /// Key-list conventions (must match the dispatcher's lookup
    /// order in `koda-core/src/tools/*.rs`):
    ///
    /// | Tool                        | Path keys                              | Pattern keys                  |
    /// |-----------------------------|----------------------------------------|-------------------------------|
    /// | `Read`/`Write`/`Edit`/`Delete` | `file_path`, `path`                 | —                             |
    /// | `Grep`                      | `file_path`, `path`, `directory`       | `search_string`, `pattern`    |
    /// | `Glob`                      | `file_path`, `path`, `directory`       | `pattern`                     |
    /// | `List`                      | `file_path`, `path`, `directory`       | —                             |
    /// | `Bash`                      | —                                      | `command`, `cmd`              |
    /// | `WebFetch`                  | —                                      | `url`                         |
    pub fn from_call(name: &str, args: &Value) -> Self {
        let kind = match name {
            "Bash" => ToolCallKind::Bash {
                command: first_string(args, &["command", "cmd"]).unwrap_or_default(),
            },
            "Read" | "Write" | "Edit" | "Delete" => ToolCallKind::Path {
                path: first_string(args, &["file_path", "path"]).unwrap_or_default(),
            },
            "Grep" => ToolCallKind::Grep {
                pattern: first_string(args, &["search_string", "pattern"]).unwrap_or_default(),
                dir: first_string(args, &["file_path", "path", "directory"])
                    .unwrap_or_else(|| ".".to_string()),
            },
            "Glob" => ToolCallKind::Glob {
                pattern: first_string(args, &["pattern"]).unwrap_or_default(),
                base: first_string(args, &["file_path", "path", "directory"]),
            },
            "List" => ToolCallKind::List {
                dir: first_string(args, &["file_path", "path", "directory"])
                    .unwrap_or_else(|| ".".to_string()),
            },
            "WebFetch" => ToolCallKind::WebFetch {
                url: first_string(args, &["url"]).unwrap_or_default(),
            },
            _ => ToolCallKind::Generic {
                value: first_string_in_object(args),
            },
        };
        Self {
            name: name.to_string(),
            kind,
        }
    }
}

/// Return the first present string value among the candidate keys.
fn first_string(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()))
}

/// Return the first string value in object-iteration order, or `None`
/// if the args aren't an object or have no string values.
///
/// Used as the `Generic` fallback for tools we don't have a
/// specialized shape for. Object-iteration order is `serde_json`'s
/// insertion-preserving order, which means the first key the tool's
/// schema declares wins — usually the most informative one.
fn first_string_in_object(args: &Value) -> Option<String> {
    args.as_object()?
        .iter()
        .find_map(|(_, v)| v.as_str().map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Bash ──────────────────────────────────────────────────────────

    #[test]
    fn bash_reads_command_key() {
        let s = ToolCallSummary::from_call("Bash", &json!({ "command": "ls -la" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Bash {
                command: "ls -la".into()
            }
        );
    }

    #[test]
    fn bash_falls_back_to_cmd_alias() {
        let s = ToolCallSummary::from_call("Bash", &json!({ "cmd": "echo hi" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Bash {
                command: "echo hi".into()
            }
        );
    }

    #[test]
    fn bash_with_no_command_yields_empty() {
        let s = ToolCallSummary::from_call("Bash", &json!({}));
        assert_eq!(
            s.kind,
            ToolCallKind::Bash {
                command: String::new()
            }
        );
    }

    // ── Path family (Read/Write/Edit/Delete) ──────────────────────────

    #[test]
    fn read_write_edit_delete_share_path_shape() {
        for name in ["Read", "Write", "Edit", "Delete"] {
            let s = ToolCallSummary::from_call(name, &json!({ "file_path": "src/foo.rs" }));
            assert_eq!(
                s.kind,
                ToolCallKind::Path {
                    path: "src/foo.rs".into()
                },
                "{name} should produce ToolCallKind::Path"
            );
        }
    }

    #[test]
    fn path_falls_back_to_path_alias() {
        let s = ToolCallSummary::from_call("Read", &json!({ "path": "legacy.rs" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Path {
                path: "legacy.rs".into()
            }
        );
    }

    // ── Grep ──────────────────────────────────────────────────────────

    #[test]
    fn grep_reads_search_string_and_file_path() {
        let s = ToolCallSummary::from_call(
            "Grep",
            &json!({ "search_string": "TODO", "file_path": "src/" }),
        );
        assert_eq!(
            s.kind,
            ToolCallKind::Grep {
                pattern: "TODO".into(),
                dir: "src/".into()
            }
        );
    }

    #[test]
    fn grep_pattern_alias_works_for_legacy_callers() {
        let s = ToolCallSummary::from_call(
            "Grep",
            &json!({ "pattern": "fn main", "directory": "src/" }),
        );
        assert_eq!(
            s.kind,
            ToolCallKind::Grep {
                pattern: "fn main".into(),
                dir: "src/".into()
            }
        );
    }

    #[test]
    fn grep_default_directory_is_dot() {
        let s = ToolCallSummary::from_call("Grep", &json!({ "search_string": "x" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Grep {
                pattern: "x".into(),
                dir: ".".into()
            }
        );
    }

    // ── Glob ──────────────────────────────────────────────────────────

    #[test]
    fn glob_with_no_base_leaves_base_none() {
        let s = ToolCallSummary::from_call("Glob", &json!({ "pattern": "*.rs" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Glob {
                pattern: "*.rs".into(),
                base: None
            }
        );
    }

    #[test]
    fn glob_surfaces_file_path_as_base_when_present() {
        let s = ToolCallSummary::from_call(
            "Glob",
            &json!({ "pattern": "*.toml", "file_path": "koda-cli/" }),
        );
        assert_eq!(
            s.kind,
            ToolCallKind::Glob {
                pattern: "*.toml".into(),
                base: Some("koda-cli/".into()),
            }
        );
    }

    // ── List ──────────────────────────────────────────────────────────

    #[test]
    fn list_default_directory_is_dot() {
        let s = ToolCallSummary::from_call("List", &json!({}));
        assert_eq!(s.kind, ToolCallKind::List { dir: ".".into() });
    }

    #[test]
    fn list_uses_file_path_key_from_schema() {
        let s = ToolCallSummary::from_call("List", &json!({ "file_path": "koda-core/src/" }));
        assert_eq!(
            s.kind,
            ToolCallKind::List {
                dir: "koda-core/src/".into()
            }
        );
    }

    // ── WebFetch ──────────────────────────────────────────────────────

    #[test]
    fn webfetch_reads_url() {
        let s = ToolCallSummary::from_call("WebFetch", &json!({ "url": "https://example.com" }));
        assert_eq!(
            s.kind,
            ToolCallKind::WebFetch {
                url: "https://example.com".into()
            }
        );
    }

    // ── Generic fallback ──────────────────────────────────────────────

    #[test]
    fn generic_picks_first_string_value_in_object_order() {
        let s = ToolCallSummary::from_call("UnknownTool", &json!({ "a": "first", "b": "second" }));
        assert_eq!(
            s.kind,
            ToolCallKind::Generic {
                value: Some("first".into())
            }
        );
    }

    #[test]
    fn generic_with_no_string_values_yields_none() {
        let s = ToolCallSummary::from_call("UnknownTool", &json!({ "n": 42 }));
        assert_eq!(s.kind, ToolCallKind::Generic { value: None });
    }

    #[test]
    fn generic_with_non_object_args_yields_none() {
        let s = ToolCallSummary::from_call("UnknownTool", &json!("just a string"));
        assert_eq!(s.kind, ToolCallKind::Generic { value: None });
    }

    // ── Pinning tests for the bug class this module exists to prevent ─

    /// Regression test for the #1099 bug class: every path-bearing
    /// tool MUST honor `file_path` (the schema-blessed key the
    /// dispatcher actually reads) before any legacy alias. Pre-fix
    /// the renderers checked obsolete keys first and silently
    /// rendered `.` for every call.
    ///
    /// This is the structural equivalent of
    /// `path_bearing_tools_render_actual_dispatch_key` in
    /// `tool_header.rs` — that test pins the renderer's output;
    /// this one pins the data layer the renderer reads from.
    #[test]
    fn path_bearing_tools_honor_file_path_key() {
        let cases = [
            (
                "List",
                json!({ "file_path": "alpha", "path": "WRONG", "directory": "WRONG" }),
                "alpha",
            ),
            (
                "Grep",
                json!({
                    "search_string": "x",
                    "file_path": "bravo",
                    "path": "WRONG",
                    "directory": "WRONG",
                }),
                "bravo",
            ),
            (
                "Glob",
                json!({ "pattern": "*", "file_path": "charlie", "path": "WRONG" }),
                "charlie",
            ),
            (
                "Read",
                json!({ "file_path": "delta", "path": "WRONG" }),
                "delta",
            ),
        ];

        for (name, args, expected) in cases {
            let s = ToolCallSummary::from_call(name, &args);
            let actual = match &s.kind {
                ToolCallKind::List { dir } => dir.clone(),
                ToolCallKind::Grep { dir, .. } => dir.clone(),
                ToolCallKind::Glob { base, .. } => base.clone().unwrap_or_default(),
                ToolCallKind::Path { path } => path.clone(),
                other => panic!("{name} produced unexpected kind {other:?}"),
            };
            assert_eq!(
                actual, expected,
                "{name}: must read `file_path` first — that's the key the dispatcher reads. \
                 Pre-#1099, renderers checked obsolete keys first and silently rendered \
                 wrong paths for every call."
            );
        }
    }
}
