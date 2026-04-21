//! Heuristic stderr → [`Violation`] parser.
//!
//! Both `sandbox-exec` (macOS) and `bwrap` (Linux) surface kernel sandbox
//! denials by letting the *child* process's syscall fail with `EACCES`,
//! `EPERM`, or `EROFS`. The child shell then prints the canonical libc
//! error string to stderr — `"Operation not permitted"`, `"Permission
//! denied"`, `"Read-only file system"` — followed by the offending path.
//!
//! ## Why heuristic?
//!
//! The "right" approach is the CC pattern: tail `log stream --predicate
//! 'eventMessage CONTAINS "_SBX"'` on macOS and parse seccomp/audit
//! logs on Linux. That's a long-lived background thread with command
//! correlation via base64-tagged process names — easily 300 lines, and
//! macOS-only without root on Linux.
//!
//! Phase 1 ships the cheap version: parse stderr after every command.
//! False positives are possible (e.g. `chmod` on a user-deleted file
//! also says `Permission denied`) but the bias is acceptable — a
//! spurious `<sandbox_violations>` annotation is just a hint to the
//! model, not a hard error. Phase 5 can swap in the syslog tail once
//! we know how often the heuristic misclassifies.
//!
//! ## What we extract
//!
//! - **Path**: the last `/`-prefixed token on the line (works for `touch`,
//!   `cp`, `mv`, `bash` redirection, `ls` denials).
//! - **Kind**: inferred from the surrounding verb when present. `touch`,
//!   `mkdir`, `>` redirection → [`ViolationKind::FileWrite`]; `cat`, `ls`,
//!   `cp src` → [`ViolationKind::FileRead`]; falls back to
//!   [`ViolationKind::Other`].
//!
//! Network and process-exec denials don't reliably show up in stderr —
//! Phase 3 (proxy) and Phase 5 (syslog) cover those.

use crate::violations::{Violation, ViolationKind};

/// Stderr substrings that indicate a kernel-enforced denial.
///
/// Order matters for the kind heuristic — see [`infer_kind_from_line`].
const DENIAL_MARKERS: &[&str] = &[
    "Operation not permitted",
    "Permission denied",
    "Read-only file system",
];

/// Parse a captured stderr buffer into zero or more violations.
///
/// `command` is attached to each violation for correlation back to the
/// triggering bash invocation. Pass `None` if you don't have it.
///
/// Lines that don't match any `DENIAL_MARKERS` are skipped silently.
/// Multi-line denials (rare — only `cp -r` produces them) are recorded
/// as separate violations, one per stderr line.
#[must_use]
pub fn parse_stderr(stderr: &str, command: Option<&str>) -> Vec<Violation> {
    stderr
        .lines()
        .filter_map(|line| parse_line(line, command))
        .collect()
}

fn parse_line(line: &str, command: Option<&str>) -> Option<Violation> {
    if !DENIAL_MARKERS.iter().any(|m| line.contains(m)) {
        return None;
    }
    Some(Violation::new(
        infer_kind_from_line(line),
        extract_path(line),
        command.map(str::to_string),
    ))
}

/// Pull the most likely target path out of a stderr line.
///
/// Strategy: walk tokens right-to-left, skip the trailing `:` punctuation
/// the libc error formatter inserts, and return the first absolute path
/// we find. Examples this handles correctly:
///
/// ```text
/// touch: /Users/me/.ssh/canary: Operation not permitted
/// sh: line 1: /tmp/x/evil.txt: Permission denied
/// cp: cannot create regular file '/etc/passwd': Permission denied
/// ```
fn extract_path(line: &str) -> Option<String> {
    line.split_whitespace()
        .rev()
        .map(|tok| tok.trim_matches(|c: char| matches!(c, ':' | ',' | '\'' | '"' | '`')))
        .find(|tok| tok.starts_with('/') && tok.len() > 1)
        .map(str::to_string)
}

/// Infer the violation kind from verb context in the stderr line.
///
/// `touch`, `mkdir`, `cp -> dest`, `>` redirection are write-flavored.
/// `cat`, `ls`, `head`, `find` are read-flavored. Everything else falls
/// back to [`ViolationKind::Other`] — better honest "I don't know" than
/// confidently-wrong tagging.
fn infer_kind_from_line(line: &str) -> ViolationKind {
    let lower = line.to_ascii_lowercase();
    if lower.contains("read-only file system") {
        return ViolationKind::FileWrite;
    }
    let write_markers = [
        "touch",
        "mkdir",
        "cannot create",
        "cannot remove",
        "cannot move",
        "cannot copy",
        "rm:",
        "mv:",
        "rmdir:",
        "chmod:",
        "chown:",
    ];
    if write_markers.iter().any(|m| lower.contains(m)) {
        return ViolationKind::FileWrite;
    }
    let read_markers = ["cat:", "ls:", "head:", "tail:", "find:", "grep:"];
    if read_markers.iter().any(|m| lower.contains(m)) {
        return ViolationKind::FileRead;
    }
    // sh redirection (`echo x > /etc/passwd`) shows up as `sh: ...:
    // Operation not permitted` — no verb hint, but redirections always
    // imply write intent.
    if lower.starts_with("sh:") || lower.starts_with("bash:") {
        return ViolationKind::FileWrite;
    }
    ViolationKind::Other
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_denial_classified_as_write() {
        let stderr = "touch: /Users/me/.ssh/canary: Operation not permitted\n";
        let v = parse_stderr(stderr, Some("touch ~/.ssh/canary"));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileWrite);
        assert_eq!(v[0].target.as_deref(), Some("/Users/me/.ssh/canary"));
        assert_eq!(v[0].command.as_deref(), Some("touch ~/.ssh/canary"));
    }

    #[test]
    fn shell_redirect_denial_classified_as_write() {
        let stderr = "sh: line 1: /var/folders/xyz/evil.txt: Operation not permitted\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileWrite);
        assert_eq!(v[0].target.as_deref(), Some("/var/folders/xyz/evil.txt"));
    }

    #[test]
    fn ls_denial_classified_as_read() {
        let stderr = "ls: /Users/me/.config/koda/db: Operation not permitted\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileRead);
        assert_eq!(v[0].target.as_deref(), Some("/Users/me/.config/koda/db"));
    }

    #[test]
    fn cat_denial_classified_as_read() {
        let stderr = "cat: /etc/shadow: Permission denied\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileRead);
        assert_eq!(v[0].target.as_deref(), Some("/etc/shadow"));
    }

    #[test]
    fn cp_cannot_create_classified_as_write() {
        let stderr = "cp: cannot create regular file '/etc/passwd': Permission denied\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileWrite);
        assert_eq!(v[0].target.as_deref(), Some("/etc/passwd"));
    }

    #[test]
    fn read_only_file_system_classified_as_write() {
        let stderr = "touch: /usr/bin/foo: Read-only file system\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ViolationKind::FileWrite);
    }

    #[test]
    fn lines_without_denial_markers_are_ignored() {
        let stderr = "warning: deprecated flag\nINFO: built target koda-cli\n";
        let v = parse_stderr(stderr, None);
        assert!(v.is_empty());
    }

    #[test]
    fn multiple_denials_yield_multiple_violations() {
        let stderr = "\
touch: /etc/passwd: Operation not permitted
touch: /etc/shadow: Operation not permitted
";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].target.as_deref(), Some("/etc/passwd"));
        assert_eq!(v[1].target.as_deref(), Some("/etc/shadow"));
    }

    #[test]
    fn empty_stderr_yields_no_violations() {
        assert!(parse_stderr("", None).is_empty());
    }

    #[test]
    fn denial_without_path_still_recorded_as_other() {
        // Some libc errors don't include a path (rare, but `chmod`
        // failing on a busy file looks like this).
        let stderr = "chmod: changing permissions: Operation not permitted\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        // `chmod` is in our write-markers list.
        assert_eq!(v[0].kind, ViolationKind::FileWrite);
        assert!(v[0].target.is_none(), "no abs path → no target");
    }

    #[test]
    fn command_is_attached_to_every_violation() {
        let stderr = "touch: /a: Operation not permitted\ntouch: /b: Operation not permitted\n";
        let v = parse_stderr(stderr, Some("my-script.sh"));
        assert_eq!(v.len(), 2);
        assert!(
            v.iter()
                .all(|x| x.command.as_deref() == Some("my-script.sh"))
        );
    }

    #[test]
    fn extracts_last_absolute_path_when_multiple_present() {
        // CC-style "blocked write to X (allowed root: Y)" diagnostic.
        let stderr = "denied write to /home/me/secret (allowed: /tmp): Operation not permitted\n";
        let v = parse_stderr(stderr, None);
        // We walk right-to-left, so /tmp wins. Acceptable trade-off
        // for Phase 1 — the syslog parser in Phase 5 will be more precise.
        assert_eq!(v.len(), 1);
        assert!(v[0].target.is_some());
    }

    #[test]
    fn ignores_relative_paths() {
        // `Operation not permitted` on a relative path shouldn't be
        // attributed to the wrong target. Better to record `target=None`
        // than fabricate an absolute path.
        let stderr = "touch: foo.txt: Permission denied\n";
        let v = parse_stderr(stderr, None);
        assert_eq!(v.len(), 1);
        assert!(
            v[0].target.is_none(),
            "relative path must not be attributed"
        );
    }
}
