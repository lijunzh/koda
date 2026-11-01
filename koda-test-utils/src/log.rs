//! Reading the log of a `koda` run from a test.
//!
//! The log's structured lines are what triage reads, so tests pin them field
//! by field rather than by substring: all the fields must be on one line.

use std::path::Path;

/// Everything koda logged for a run whose config dir was `config`.
pub fn read_log(config: &Path) -> String {
    std::fs::read_dir(config.join("koda/logs"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("koda-"))
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

/// The first line with `message` and every one of `fields`.
///
/// A field is `name=value`, or `name=` for one whose value varies. Panics with
/// the lines carrying `message`, so a failure shows what was logged instead.
pub fn log_line<'a>(log: &'a str, message: &str, fields: &[&str]) -> &'a str {
    let has_field = |line: &str, field: &str| {
        line.split_whitespace().any(|token| {
            if field.ends_with('=') {
                token.starts_with(field)
            } else {
                token == field
            }
        })
    };
    let mut candidates = log.lines().filter(|line| line.contains(message));
    candidates
        .find(|line| fields.iter().all(|field| has_field(line, field)))
        .unwrap_or_else(|| {
            let seen: Vec<&str> = log.lines().filter(|l| l.contains(message)).collect();
            panic!("no `{message}` line with {fields:?}; saw {seen:#?}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "INFO model call purpose=\"turn\" tool_calls=2\n\
                       INFO model call purpose=\"compaction\" tool_calls=0\n";

    #[test]
    fn all_fields_must_be_on_one_line() {
        let line = log_line(
            LOG,
            "model call",
            &["purpose=\"compaction\"", "tool_calls="],
        );
        assert!(line.contains("tool_calls=0"));
    }

    #[test]
    #[should_panic(expected = "no `model call` line")]
    fn fields_on_different_lines_do_not_match() {
        log_line(
            LOG,
            "model call",
            &["purpose=\"compaction\"", "tool_calls=2"],
        );
    }

    #[test]
    #[should_panic(expected = "no `model call` line")]
    fn a_value_must_match_whole() {
        log_line(LOG, "model call", &["tool_calls=20"]);
    }
}
