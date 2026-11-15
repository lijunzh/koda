//! The choices `/feedback` offers, as Codex's feedback view does: what kind
//! of report, then whether to include the logs and session history.

use super::dropdown::DropdownItem;

/// One choice: what it's called, what it means, and the value it stands
/// for (a classification, or `yes` / `no` for the logs).
#[derive(Clone, Debug)]
pub struct FeedbackItem {
    pub label: &'static str,
    pub description: &'static str,
    pub value: &'static str,
}

impl DropdownItem for FeedbackItem {
    fn label(&self) -> &str {
        self.label
    }
    fn description(&self) -> String {
        self.description.to_string()
    }
    fn matches_filter(&self, filter: &str) -> bool {
        self.label.contains(&filter.to_lowercase())
    }
}

/// Codex's categories and descriptions, less `safety check`: that is for
/// OpenAI's own safety checks, and Koda has none.
pub fn categories() -> Vec<FeedbackItem> {
    vec![
        FeedbackItem {
            label: "bug",
            description: "Crash, error message, hang, or broken UI/behavior",
            value: "bug",
        },
        FeedbackItem {
            label: "bad result",
            description: "Output was off-target, incorrect, incomplete, or unhelpful",
            value: "bad_result",
        },
        FeedbackItem {
            label: "good result",
            description: "Helpful, correct, high-quality, or delightful result worth celebrating",
            value: "good_result",
        },
        FeedbackItem {
            label: "other",
            description: "Slowness, feature suggestion, UX feedback, or anything else",
            value: "other",
        },
    ]
}

/// Codex's question: include the logs and session history?
pub fn consent() -> Vec<FeedbackItem> {
    vec![
        FeedbackItem {
            label: "yes",
            description: "Logs, this session's stored history and its sub-agents', diagnostics",
            value: "yes",
        },
        FeedbackItem {
            label: "no",
            description: "Only the report: the category and your note",
            value: "no",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_use_codex_s_names() {
        let values: Vec<_> = categories().iter().map(|c| c.value).collect();
        assert_eq!(values, koda_client::feedback::CLASSIFICATIONS);
    }
}
