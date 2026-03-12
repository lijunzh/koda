//! Output truncation for display.
//!
//! When tool output exceeds a threshold, we show the first N lines (head)
//! and the last N lines (tail) with a separator indicating how many lines
//! were hidden. This keeps the UI scannable while preserving the most
//! important parts (beginning for context, end for results).

/// Default threshold: truncate if output exceeds this many lines.
pub const TRUNCATE_THRESHOLD: usize = 50;
/// Number of head lines to keep.
const HEAD_LINES: usize = 20;
/// Number of tail lines to keep.
const TAIL_LINES: usize = 20;

/// Result of truncation.
#[derive(Debug, PartialEq)]
pub enum Truncated<'a> {
    /// Output was short enough — no truncation needed.
    Full(&'a str),
    /// Output was truncated into head + tail.
    Split {
        /// First N lines of output.
        head: Vec<&'a str>,
        /// Last N lines of output.
        tail: Vec<&'a str>,
        /// Number of lines hidden between head and tail.
        hidden: usize,
        /// Total line count before truncation.
        total: usize,
    },
}

/// Truncate long output for display, keeping head and tail lines.
///
/// Returns `Truncated::Full` if output is within the threshold,
/// or `Truncated::Split` with head/tail lines and hidden count.
pub fn truncate_for_display(output: &str) -> Truncated<'_> {
    let lines: Vec<&str> = output.lines().collect();
    let total = lines.len();

    if total <= TRUNCATE_THRESHOLD {
        return Truncated::Full(output);
    }

    let head = lines[..HEAD_LINES].to_vec();
    let tail = lines[total - TAIL_LINES..].to_vec();
    let hidden = total - HEAD_LINES - TAIL_LINES;

    Truncated::Split {
        head,
        tail,
        hidden,
        total,
    }
}

/// Format a separator line for the truncation gap.
pub fn separator(hidden: usize, total: usize) -> String {
    format!(
        "  \u{2502} \u{2500}\u{2500}\u{2500} {hidden} lines hidden ({total} total, use /expand to see all) \u{2500}\u{2500}\u{2500}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_output_not_truncated() {
        let output = "line1\nline2\nline3";
        assert_eq!(truncate_for_display(output), Truncated::Full(output));
    }

    #[test]
    fn test_exactly_at_threshold() {
        let lines: Vec<String> = (0..TRUNCATE_THRESHOLD)
            .map(|i| format!("line {i}"))
            .collect();
        let output = lines.join("\n");
        assert_eq!(truncate_for_display(&output), Truncated::Full(&output));
    }

    #[test]
    fn test_over_threshold_splits() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");
        match truncate_for_display(&output) {
            Truncated::Split {
                head,
                tail,
                hidden,
                total,
            } => {
                assert_eq!(head.len(), HEAD_LINES);
                assert_eq!(tail.len(), TAIL_LINES);
                assert_eq!(hidden, 100 - HEAD_LINES - TAIL_LINES);
                assert_eq!(total, 100);
                assert_eq!(head[0], "line 0");
                assert_eq!(head[HEAD_LINES - 1], &format!("line {}", HEAD_LINES - 1));
                assert_eq!(tail[0], &format!("line {}", 100 - TAIL_LINES));
                assert_eq!(tail[TAIL_LINES - 1], "line 99");
            }
            Truncated::Full(_) => panic!("Expected Split"),
        }
    }

    #[test]
    fn test_just_over_threshold() {
        let lines: Vec<String> = (0..51).map(|i| format!("line {i}")).collect();
        let output = lines.join("\n");
        match truncate_for_display(&output) {
            Truncated::Split {
                head,
                tail,
                hidden,
                total,
            } => {
                assert_eq!(head.len(), HEAD_LINES);
                assert_eq!(tail.len(), TAIL_LINES);
                assert_eq!(hidden, 11); // 51 - 20 - 20
                assert_eq!(total, 51);
            }
            Truncated::Full(_) => panic!("Expected Split"),
        }
    }

    #[test]
    fn test_separator_format() {
        let sep = separator(60, 100);
        assert!(sep.contains("60 lines hidden"));
        assert!(sep.contains("100 total"));
        assert!(sep.contains("/expand"));
    }
}
