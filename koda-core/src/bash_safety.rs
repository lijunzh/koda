//! Bash command safety classification.
//!
//! Classifies shell commands by effect: ReadOnly (auto-approve),
//! LocalMutation (default for unknown), or Destructive (always confirm).
//!
//! Three-phase pipeline (#807):
//!
//! 1. **Raw structural check** — `RAW_DANGER_PATTERNS` matched against the
//!    quote-stripped string: backticks, `$(`, pipes-to-shells, fork-bomb syntax.
//! 2. **Write side-effect check** — quote-aware `>` / `>>` / `| tee` detection.
//! 3. **Token-level check** — each pipeline segment is tokenised with
//!    [`shlex::split`] (POSIX); tokens matched against `DANGER_CHECKS` (typed
//!    enum, not flat substrings). Unparseable segments fail-open → LocalMutation.
//!
//! This replaces the old `strip_quoted_strings().contains(pat)` approach, which
//! produced false positives when a dangerous-looking string appeared as a quoted
//! argument (e.g. `grep "cargo publish" .`) and missed `$'...'` ANSI-C quoting.
//!
//! The LLM is semi-trusted (not adversarial). Classification is a UX layer
//! (auto-approve vs prompt), not a security enforcement boundary. Kernel-level
//! sandboxing is tracked separately in DESIGN.md.

use crate::tools::ToolEffect;

// ── Read-only allowlist ───────────────────────────────────────────────────────

/// Commands that are truly read-only — no filesystem writes, no state changes.
const READ_ONLY_PREFIXES: &[&str] = &[
    // File inspection
    "cat ",
    "head ",
    "tail ",
    "less ",
    "more ",
    "wc ",
    "file ",
    "stat ",
    "bat ",
    // Directory listing
    "ls",
    "tree",
    "du ",
    "df",
    "pwd",
    // Search
    "grep ",
    "rg ",
    "ag ",
    "find ",
    "fd ",
    "fzf",
    // System info
    "echo ",
    "printf ",
    "whoami",
    "hostname",
    "uname",
    "date",
    "which ",
    "type ",
    "command -v ",
    "env",
    "printenv",
    // Version checks
    "rustc --version",
    "node --version",
    "npm --version",
    "python --version",
    "python3 --version",
    // Git read-only
    "git status",
    "git log",
    "git diff",
    "git branch",
    "git show",
    "git remote",
    "git stash list",
    "git tag",
    "git describe",
    "git rev-parse",
    "git ls-files",
    "git blame",
    // Docker read-only
    "docker ps",
    "docker images",
    "docker logs",
    "docker compose ps",
    "docker compose logs",
    // Text processing (stdout-only; sed -i caught in DANGER_CHECKS)
    "sort ",
    "uniq ",
    "cut ",
    "awk ",
    "sed ",
    "tr ",
    "diff ",
    "jq ",
    "yq ",
    "xargs ",
    "dirname ",
    "basename ",
    "realpath ",
    "readlink ",
    // Misc
    "tput ",
    "true",
    "false",
    "test ",
    "[ ",
    // GitHub CLI read-only (#518, #525)
    "gh issue view",
    "gh issue list",
    "gh issue status",
    "gh pr view",
    "gh pr list",
    "gh pr status",
    "gh pr checks",
    "gh pr diff",
    "gh repo view",
    "gh repo clone",
    "gh release list",
    "gh release view",
    "gh run view",
    "gh run list",
    "gh run watch",
];

// ── Token-level danger checks ─────────────────────────────────────────────────

/// Structured representation of a dangerous command pattern.
///
/// Each variant is checked against the shlex token array for a pipeline
/// segment, avoiding the substring-matching false-positives of the old design.
#[derive(Debug, Clone, Copy)]
enum DangerCheck {
    /// Any invocation of this command is Destructive: `rm`, `sudo`, …
    Cmd(&'static str),
    /// Command with a specific flag anywhere in args: `sed -i`, `python -c`, …
    CmdFlag(&'static str, &'static str),
    /// Command with an exact subcommand: `npm publish`, `cargo publish`, …
    CmdSub(&'static str, &'static str),
    /// Command + subcommand + flag anywhere in remaining args: `git push -f`
    CmdSubFlag(&'static str, &'static str, &'static str),
    /// Command + subcommand + exact second token: `gh pr merge`, …
    CmdSubSub(&'static str, &'static str, &'static str),
}

/// Returns `true` if token `t` matches `flag` — exact, long-flag, or
/// combined short-flag (e.g. `-f` matches `-fd`, `-fdc`).
fn flag_matches(t: &str, flag: &str) -> bool {
    if t == flag {
        return true;
    }
    // Combined short flags: `-fd` should match flag `-f`
    if flag.len() == 2 && flag.starts_with('-') && t.starts_with('-') && !t.starts_with("--") {
        let ch = flag.chars().nth(1).unwrap();
        return t[1..].contains(ch);
    }
    false
}

impl DangerCheck {
    fn matches(&self, tokens: &[String]) -> bool {
        use DangerCheck::*;
        let Some(cmd) = tokens.first() else {
            return false;
        };
        match *self {
            Cmd(c) => cmd == c,
            CmdFlag(c, flag) => cmd == c && tokens[1..].iter().any(|t| flag_matches(t, flag)),
            CmdSub(c, sub) => cmd == c && tokens.get(1).map(|s| s.as_str()) == Some(sub),
            CmdSubFlag(c, sub, flag) => {
                cmd == c
                    && tokens.get(1).map(|s| s.as_str()) == Some(sub)
                    && tokens[2..].iter().any(|t| flag_matches(t, flag))
            }
            CmdSubSub(c, sub, sub2) => {
                cmd == c
                    && tokens.get(1).map(|s| s.as_str()) == Some(sub)
                    && tokens.get(2).map(|s| s.as_str()) == Some(sub2)
            }
        }
    }
}

const DANGER_CHECKS: &[DangerCheck] = &[
    // ── Destructive file operations ──────────────────────────────────────────
    DangerCheck::Cmd("rm"),
    DangerCheck::Cmd("rmdir"),
    // ── Privilege escalation ─────────────────────────────────────────────────
    DangerCheck::Cmd("sudo"),
    DangerCheck::Cmd("su"),
    // ── Low-level disk operations ────────────────────────────────────────────
    DangerCheck::Cmd("dd"),
    DangerCheck::Cmd("mkfs"),
    DangerCheck::Cmd("fdisk"),
    // ── Permission / ownership changes ───────────────────────────────────────
    DangerCheck::Cmd("chmod"),
    DangerCheck::Cmd("chown"),
    // ── Process control ──────────────────────────────────────────────────────
    DangerCheck::Cmd("kill"),
    DangerCheck::Cmd("killall"),
    DangerCheck::Cmd("pkill"),
    // ── Arbitrary code execution ─────────────────────────────────────────────
    DangerCheck::Cmd("eval"),
    // ── System control ───────────────────────────────────────────────────────
    DangerCheck::Cmd("reboot"),
    DangerCheck::Cmd("shutdown"),
    DangerCheck::Cmd("halt"),
    // ── In-place file edits ──────────────────────────────────────────────────
    DangerCheck::CmdFlag("sed", "-i"),
    DangerCheck::CmdFlag("sed", "--in-place"),
    // ── Interpreter inline execution (prompt-injection vector) ───────────────
    DangerCheck::CmdFlag("python", "-c"),
    DangerCheck::CmdFlag("python3", "-c"),
    DangerCheck::CmdFlag("perl", "-e"),
    DangerCheck::CmdFlag("ruby", "-e"),
    DangerCheck::CmdFlag("node", "-e"),
    // ── Nested shells (bypass classifier) ────────────────────────────────────
    DangerCheck::CmdFlag("sh", "-c"),
    DangerCheck::CmdFlag("bash", "-c"),
    DangerCheck::CmdFlag("zsh", "-c"),
    // ── Package publishing ────────────────────────────────────────────────────
    DangerCheck::CmdSub("npm", "publish"),
    DangerCheck::CmdSub("cargo", "publish"),
    // ── Destructive git ───────────────────────────────────────────────────────
    DangerCheck::CmdSubFlag("git", "push", "-f"),
    DangerCheck::CmdSubFlag("git", "push", "--force"),
    DangerCheck::CmdSubFlag("git", "reset", "--hard"),
    DangerCheck::CmdSubFlag("git", "clean", "-f"), // also matches -fd, -fdc, …
    // ── GitHub CLI destructive (#518, #525) ───────────────────────────────────
    DangerCheck::CmdSubSub("gh", "pr", "merge"),
    DangerCheck::CmdSubSub("gh", "issue", "delete"),
    DangerCheck::CmdSubSub("gh", "repo", "delete"),
    DangerCheck::CmdSubSub("gh", "release", "delete"),
    DangerCheck::CmdSub("gh", "api"),
    DangerCheck::CmdSub("gh", "auth"),
];

// ── Raw structural patterns ───────────────────────────────────────────────────

/// Shell metacharacters that shlex cannot represent as distinct tokens.
///
/// Matched against [`strip_quoted_strings`] output so patterns inside quoted
/// arguments are ignored (`grep "| sh" file` is safe).
const RAW_DANGER_PATTERNS: &[&str] = &[
    "$(",   // command substitution
    "`",    // backtick command substitution
    "| sh", // pipe to shell interpreter
    "| bash", "| zsh", "> /dev/", // device writes (>/dev/null is exempted in Phase 2)
    "(){",     // fork bomb
    "() {",
];

// ── Main classifier ───────────────────────────────────────────────────────────

/// Classify a bash command by its side-effect severity.
///
/// Returns the *most dangerous* effect found across all pipeline/chain segments:
/// 1. Raw structural patterns on quote-stripped string → Destructive
/// 2. Write side-effects (`>`, `>>`, `| tee`) → LocalMutation
/// 3. Per-segment shlex tokenisation vs `DANGER_CHECKS` → Destructive
/// 4. Per-segment allowlist vs `READ_ONLY_PREFIXES` → ReadOnly / LocalMutation
///
/// Segments that fail shlex tokenisation fail-open to `LocalMutation`.
///
/// # Examples
///
/// ```
/// use koda_core::bash_safety::classify_bash_command;
/// use koda_core::tools::ToolEffect;
///
/// assert_eq!(classify_bash_command("ls -la"), ToolEffect::ReadOnly);
/// assert_eq!(classify_bash_command("git status"), ToolEffect::ReadOnly);
/// assert_eq!(classify_bash_command("cargo build"), ToolEffect::LocalMutation);
/// assert_eq!(classify_bash_command("rm -rf /"), ToolEffect::Destructive);
/// ```
pub fn classify_bash_command(command: &str) -> ToolEffect {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ToolEffect::ReadOnly;
    }

    // Phase 1 — raw structural patterns on quote-stripped text.
    let unquoted = strip_quoted_strings(trimmed);
    for pat in RAW_DANGER_PATTERNS {
        if unquoted.contains(pat) {
            return ToolEffect::Destructive;
        }
    }

    // Phase 2 — write side-effects.
    if has_write_side_effect(trimmed) {
        return ToolEffect::LocalMutation;
    }

    // Phase 3 — per-segment token-level classification.
    // We must check ALL segments before short-circuiting on LocalMutation
    // because a later segment may be Destructive.
    let segments = split_command_segments(trimmed);
    let mut worst = ToolEffect::ReadOnly;

    for seg in &segments {
        let effect = classify_segment(seg);
        match effect {
            ToolEffect::Destructive => return ToolEffect::Destructive,
            ToolEffect::LocalMutation => worst = ToolEffect::LocalMutation,
            _ => {}
        }
    }

    worst
}

/// Classify a single pipeline/chain segment using shlex tokenisation.
///
/// Returns `LocalMutation` on parse failure (fail-open — the user sees the
/// prompt and can decide; we never auto-approve unparseable syntax).
fn classify_segment(segment: &str) -> ToolEffect {
    let seg = strip_env_vars(segment.trim());
    let seg = strip_redirections(&seg);
    let seg = seg.trim().to_string();

    if seg.is_empty() {
        return ToolEffect::ReadOnly;
    }

    // Tokenise with POSIX shlex. Returns None for unterminated quotes,
    // complex bash syntax, etc. → fail-open.
    let tokens = match shlex::split(&seg) {
        Some(t) if !t.is_empty() => t,
        _ => return ToolEffect::LocalMutation,
    };

    // Check against structured danger patterns.
    for check in DANGER_CHECKS {
        if check.matches(&tokens) {
            return ToolEffect::Destructive;
        }
    }

    // Fall back to read-only allowlist.
    // Join tokens with single spaces to normalise irregular whitespace.
    let canonical = tokens.join(" ");
    if matches_prefix_list(&canonical, READ_ONLY_PREFIXES) {
        ToolEffect::ReadOnly
    } else {
        ToolEffect::LocalMutation
    }
}

// ── Write side-effect detection ───────────────────────────────────────────────

/// Detect write side-effects: `>`, `>>` (except `>/dev/null`, `2>&1`), `| tee`.
fn has_write_side_effect(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_sq = false;
    let mut in_dq = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && !in_dq {
            in_sq = !in_sq;
        } else if c == '"' && !in_sq {
            in_dq = !in_dq;
        } else if !in_sq && !in_dq && c == '>' {
            let before = if i > 0 { chars[i - 1] } else { ' ' };
            if before == '&' {
                i += 1;
                continue;
            }
            let after: String = chars[i + 1..].iter().collect();
            let after_trimmed = after.trim_start();
            if after_trimmed.starts_with("/dev/null")
                || after_trimmed.starts_with("&1")
                || after_trimmed.starts_with("&2")
            {
                i += 1;
                continue;
            }
            return true;
        }
        i += 1;
    }

    // `| tee` check
    let segments = split_command_segments(command);
    for (idx, seg) in segments.iter().enumerate() {
        if idx > 0 {
            let t = seg.trim();
            if t.starts_with("tee ") || t == "tee" {
                return true;
            }
        }
    }

    false
}

// ── Public helpers (also used by bash_path_lint) ──────────────────────────────

/// Check if a segment matches any entry in a prefix list.
///
/// Each prefix is treated as a bare command (or command head). A trailing space
/// in the prefix is **decorative only** — historically used to flag entries that
/// 'normally take an argument' — but the matcher always accepts:
///
/// 1. exact match (e.g. bare `sort` at end of `grep ... | sort`)
/// 2. match followed by a space (e.g. `sort -u`)
/// 3. match followed by a tab
///
/// This avoids substring false-positives like `sortfoo` matching `sort`, while
/// correctly classifying bare invocations of stdin-consuming filters (#944).
fn matches_prefix_list(seg: &str, prefixes: &[&str]) -> bool {
    for prefix in prefixes {
        let bare = prefix.trim_end();
        if seg == bare
            || seg.starts_with(&format!("{bare} "))
            || seg.starts_with(&format!("{bare}\t"))
        {
            return true;
        }
    }
    false
}

/// Split a command into segments on `|`, `&&`, `||`, `;`.
/// Respects single and double quotes.
///
/// # Examples
///
/// ```
/// use koda_core::bash_safety::split_command_segments;
///
/// assert_eq!(split_command_segments("ls | grep foo"), vec!["ls ", " grep foo"]);
/// assert_eq!(split_command_segments("a && b || c"), vec!["a ", " b ", " c"]);
/// ```
pub fn split_command_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if !in_single_quote && !in_double_quote {
            let sep_len = if (c == '|' || c == '&') && i + 1 < chars.len() && chars[i + 1] == c {
                2 // || or &&
            } else if c == '|' || c == ';' {
                1
            } else {
                0
            };
            if sep_len > 0 {
                segments.push(&command[start..i]);
                i += sep_len;
                start = i;
                continue;
            }
        }
        i += 1;
    }
    if start < chars.len() {
        segments.push(&command[start..]);
    }
    segments
}

/// Replace content inside single and double quotes with spaces.
///
/// Used before `RAW_DANGER_PATTERNS` matching to suppress false positives
/// from quoted arguments (e.g. `grep "cargo publish" .`).
pub fn strip_quoted_strings(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            result.push(c);
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '\'' {
                    result.push(c);
                    found_close = true;
                    break;
                }
                result.push(' ');
            }
            let _ = found_close;
        } else if c == '"' {
            result.push(c);
            let mut found_close = false;
            while let Some(inner) = chars.next() {
                if inner == '\\' {
                    result.push(' ');
                    if chars.next().is_some() {
                        result.push(' ');
                    }
                    continue;
                }
                if inner == '"' {
                    result.push(c);
                    found_close = true;
                    break;
                }
                result.push(' ');
            }
            let _ = found_close;
        } else {
            result.push(c);
        }
    }
    result
}

/// Strip leading environment variable assignments (`FOO=bar command`).
///
/// # Examples
///
/// ```
/// use koda_core::bash_safety::strip_env_vars;
///
/// assert_eq!(strip_env_vars("FOO=bar cargo build"), "cargo build");
/// assert_eq!(strip_env_vars("ls -la"), "ls -la");
/// ```
pub fn strip_env_vars(segment: &str) -> String {
    let mut rest = segment;
    loop {
        let trimmed = rest.trim_start();
        if let Some(eq_pos) = trimmed.find('=') {
            let before_eq = &trimmed[..eq_pos];
            if !before_eq.is_empty()
                && before_eq
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let after_eq = &trimmed[eq_pos + 1..];
                if let Some(space_pos) = find_unquoted_space(after_eq) {
                    rest = &after_eq[space_pos..];
                    continue;
                }
            }
        }
        return trimmed.to_string();
    }
}

/// Strip common shell redirections so they don't confuse the allowlist matcher.
fn strip_redirections(segment: &str) -> String {
    let mut result = segment.to_string();
    for pat in ["2>&1", "2>/dev/null", ">/dev/null", "</dev/null"] {
        result = result.replace(pat, "");
    }
    result
}

/// Position of the first unquoted space in `s`.
fn find_unquoted_space(s: &str) -> Option<usize> {
    let mut in_sq = false;
    let mut in_dq = false;
    for (i, c) in s.chars().enumerate() {
        match c {
            '\'' if !in_dq => in_sq = !in_sq,
            '"' if !in_sq => in_dq = !in_dq,
            ' ' | '\t' if !in_sq && !in_dq => return Some(i),
            _ => {}
        }
    }
    None
}

// ── Internal unit tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // flag_matches

    #[test]
    fn test_flag_matches_exact() {
        assert!(flag_matches("-i", "-i"));
        assert!(flag_matches("--force", "--force"));
        assert!(!flag_matches("-n", "-i"));
        assert!(!flag_matches("--force", "-f"));
    }

    #[test]
    fn test_flag_matches_combined_short() {
        assert!(flag_matches("-fd", "-f"));
        assert!(flag_matches("-fdc", "-f"));
        assert!(!flag_matches("-nd", "-f"));
        assert!(!flag_matches("--force", "-f"));
    }

    // DangerCheck::matches

    #[test]
    fn test_danger_check_cmd() {
        let t = |s: &str| s.to_string();
        let rm = vec![t("rm"), t("-rf"), t("/")];
        assert!(DangerCheck::Cmd("rm").matches(&rm));
        assert!(!DangerCheck::Cmd("ls").matches(&rm));
    }

    #[test]
    fn test_danger_check_cmd_flag() {
        let t = |s: &str| s.to_string();
        let sed_i = vec![t("sed"), t("-i"), t("s/a/b/")];
        assert!(DangerCheck::CmdFlag("sed", "-i").matches(&sed_i));
        assert!(!DangerCheck::CmdFlag("sed", "--in-place").matches(&sed_i));
    }

    #[test]
    fn test_danger_check_combined_flag() {
        let t = |s: &str| s.to_string();
        let git_clean_fd = vec![t("git"), t("clean"), t("-fd")];
        assert!(DangerCheck::CmdSubFlag("git", "clean", "-f").matches(&git_clean_fd));
        let git_clean_n = vec![t("git"), t("clean"), t("-nd")];
        assert!(!DangerCheck::CmdSubFlag("git", "clean", "-f").matches(&git_clean_n));
    }

    #[test]
    fn test_danger_check_cmd_sub_sub() {
        let t = |s: &str| s.to_string();
        let merge = vec![t("gh"), t("pr"), t("merge"), t("42")];
        assert!(DangerCheck::CmdSubSub("gh", "pr", "merge").matches(&merge));
        assert!(!DangerCheck::CmdSubSub("gh", "pr", "view").matches(&merge));
    }

    // split_command_segments

    #[test]
    fn test_split_pipe() {
        let segs = split_command_segments("cat file | grep pattern");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].trim(), "cat file");
        assert_eq!(segs[1].trim(), "grep pattern");
    }

    #[test]
    fn test_split_chain_and_semicolon() {
        assert_eq!(split_command_segments("cargo build && cargo test").len(), 2);
        assert_eq!(split_command_segments("echo a; echo b; echo c").len(), 3);
    }

    #[test]
    fn test_split_respects_quotes() {
        let segs = split_command_segments("echo 'a | b' | grep x");
        assert_eq!(segs.len(), 2);
        assert!(segs[0].contains("'a | b'"));
    }

    // strip_quoted_strings

    #[test]
    fn test_strip_quoted_backslash_escaped() {
        assert_eq!(
            strip_quoted_strings(r#"echo "it\"s fine" ; ls"#),
            r#"echo "          " ; ls"#,
        );
        let stripped = strip_quoted_strings(r#"echo "safe\" ; rm -rf /""#);
        assert!(!stripped.contains("rm -rf"));
    }

    // strip_env_vars

    #[test]
    fn test_strip_env_vars_basic() {
        assert_eq!(strip_env_vars("FOO=bar cargo build"), "cargo build");
        assert_eq!(strip_env_vars("ls -la"), "ls -la");
    }

    // matches_prefix_list

    /// Bare commands at the end of a pipeline must match prefixes that
    /// historically used a trailing-space convention (#944 regression test).
    #[test]
    fn test_matches_prefix_list_bare_command() {
        let prefixes = &["sort ", "wc ", "uniq ", "cat ", "grep "];
        for cmd in ["sort", "wc", "uniq", "cat", "grep"] {
            assert!(
                matches_prefix_list(cmd, prefixes),
                "bare `{cmd}` should match the prefix list"
            );
        }
    }

    /// With-argument forms must still match (no regression).
    #[test]
    fn test_matches_prefix_list_with_args() {
        let prefixes = &["sort ", "wc ", "grep "];
        for cmd in ["sort -u", "wc -l", "grep -i foo"] {
            assert!(matches_prefix_list(cmd, prefixes), "`{cmd}` should match");
        }
    }

    /// Tab separator (rare but legal) still matches.
    #[test]
    fn test_matches_prefix_list_tab_separator() {
        let prefixes = &["sort "];
        assert!(matches_prefix_list("sort\t-u", prefixes));
    }

    /// Substring false-positives are still rejected (the original purpose of
    /// the trailing-space convention).
    #[test]
    fn test_matches_prefix_list_no_substring_false_positive() {
        let prefixes = &["sort ", "cat ", "ls"];
        for cmd in ["sortfoo", "catalogue", "lsof", "sortir"] {
            assert!(
                !matches_prefix_list(cmd, prefixes),
                "`{cmd}` must not match (substring false-positive)"
            );
        }
    }

    // classify_bash_command (#944)

    /// Bare read-only commands at the end of a pipeline must auto-approve.
    /// Repro table from #944 plus a sweep of common stdin-consuming filters.
    #[test]
    fn test_classify_bare_pipeline_tail_is_read_only() {
        let cases = [
            // From #944 repro table:
            "sort",
            "ls | sort",
            "echo hi | wc",
            "cat file | uniq",
            // Real-world example from the bug report:
            "grep -c \"^pub fn find_matches\" src/properties/*.rs | sort",
            // Other common bare stdin filters:
            "echo hi | cat",
            "echo hi | head",
            "echo hi | tail",
            "echo hi | sed",
            "echo hi | awk",
            "echo hi | tr",
            "echo hi | jq",
            "echo hi | xargs",
            "ls | wc",
            "find . | sort | uniq",
        ];
        for cmd in cases {
            assert_eq!(
                classify_bash_command(cmd),
                ToolEffect::ReadOnly,
                "`{cmd}` should classify as ReadOnly",
            );
        }
    }

    /// With-argument variants must still classify as ReadOnly (no regression).
    #[test]
    fn test_classify_pipeline_tail_with_args_still_read_only() {
        let cases = [
            "ls | sort -u",
            "echo hi | wc -l",
            "cat file | uniq -c",
            "find . | head -20",
            "git log | grep WIP",
        ];
        for cmd in cases {
            assert_eq!(
                classify_bash_command(cmd),
                ToolEffect::ReadOnly,
                "`{cmd}` should classify as ReadOnly",
            );
        }
    }

    /// Mutating commands at the pipeline tail must NOT be misclassified as
    /// ReadOnly by the broader matcher.
    #[test]
    fn test_classify_mutating_pipeline_tail_not_read_only() {
        // `tee` writes — caught by has_write_side_effect (Phase 2),
        // not segment classification, but verify the end-to-end result.
        for cmd in [
            "echo content | tee output.txt",
            "echo content | tee",
            "ls > files.txt",
        ] {
            assert_ne!(
                classify_bash_command(cmd),
                ToolEffect::ReadOnly,
                "`{cmd}` must not be ReadOnly (has write side effect)",
            );
        }
    }

    /// Bare commands NOT in the read-only list still classify as
    /// LocalMutation — the matcher widening doesn't leak unrelated commands.
    #[test]
    fn test_classify_bare_unknown_command_not_read_only() {
        for cmd in ["cargo", "npm", "make", "docker"] {
            assert_ne!(
                classify_bash_command(cmd),
                ToolEffect::ReadOnly,
                "bare `{cmd}` must not be misclassified as ReadOnly",
            );
        }
    }
}
