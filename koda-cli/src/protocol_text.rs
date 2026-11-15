//! The words a front end shows for protocol events that carry state but no
//! text of their own: a compaction, the loop guard, a plan's change. Shared
//! by `koda exec`'s text output and the TUI, so both say the same.

use koda_client::protocol::item::{CompactionStatus, CompactionTrigger, Item};
use koda_client::protocol::turn::{LoopGuardAction, PlanStep};

/// Whether a line is news or a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Something a person may want to know.
    Info,
    /// Something that went wrong or needs a look.
    Warn,
}

/// What a `contextCompaction` item says as it starts.
pub fn compaction_started(item: &Item) -> Option<(Tone, String)> {
    let Item::ContextCompaction {
        trigger,
        context_percent,
        ..
    } = item
    else {
        return None;
    };
    Some(match (trigger, context_percent) {
        (CompactionTrigger::Overflow, _) => (
            Tone::Warn,
            "\u{26a0}\u{fe0f} Provider rejected request (context overflow). Compacting and retrying..."
                .to_string(),
        ),
        (_, Some(pct)) => (
            Tone::Info,
            format!("\u{1f4e6} Context at {pct}% \u{2014} compacting..."),
        ),
        (_, None) => (Tone::Info, "\u{1f4e6} Compacting...".to_string()),
    })
}

/// What a `contextCompaction` item says once it is over.
pub fn compaction_done(item: &Item) -> Option<(Tone, String)> {
    let Item::ContextCompaction {
        status,
        messages_removed,
        summary_tokens,
        reason,
        ..
    } = item
    else {
        return None;
    };
    let removed = messages_removed.unwrap_or(0);
    let reason = reason.as_deref().unwrap_or("no reason given");
    Some(match status {
        CompactionStatus::InProgress => return None,
        CompactionStatus::Completed => match summary_tokens {
            Some(tokens) => (
                Tone::Info,
                format!("\u{2705} Compacted {removed} messages (~{tokens} token summary)"),
            ),
            None => (
                Tone::Warn,
                format!("\u{26a0}\u{fe0f} Dropped {removed} older messages without a summary"),
            ),
        },
        CompactionStatus::Skipped => (Tone::Info, format!("Not compacting: {reason}")),
        CompactionStatus::Failed => (Tone::Warn, format!("Compact failed: {reason}")),
    })
}

/// What `turn/loopGuard` says.
pub fn loop_guard(action: LoopGuardAction, detail: &str) -> String {
    match action {
        LoopGuardAction::Feedback => {
            format!("Loop detected: {detail}. Asking the model to change course.")
        }
        LoopGuardAction::Stop => format!(
            "Loop guard: {detail}. Stopping; the model may not be able to do this task. \
             Try a stronger model, or send a follow-up to continue."
        ),
    }
}

/// How a plan changed: steps added, changed (same text, new status) and
/// removed, against the plan before it.
pub fn plan_change(before: &[PlanStep], after: &[PlanStep]) -> (usize, usize, usize) {
    let find = |steps: &[PlanStep], text: &str| steps.iter().find(|s| s.step == text).cloned();
    let added = after
        .iter()
        .filter(|s| find(before, &s.step).is_none())
        .count();
    let changed = after
        .iter()
        .filter(|s| find(before, &s.step).is_some_and(|b| b.status != s.status))
        .count();
    let removed = before
        .iter()
        .filter(|s| find(after, &s.step).is_none())
        .count();
    (added, changed, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koda_client::protocol::turn::PlanStepStatus;

    fn compaction(status: CompactionStatus, tokens: Option<u64>) -> Item {
        Item::ContextCompaction {
            id: "c1".into(),
            trigger: CompactionTrigger::Threshold,
            status,
            context_percent: Some(91),
            messages_removed: Some(12),
            summary_tokens: tokens,
            reason: Some("Conversation is too short".into()),
        }
    }

    #[test]
    fn a_compaction_says_what_it_did() {
        let started = compaction(CompactionStatus::InProgress, None);
        assert_eq!(
            compaction_started(&started).unwrap().1,
            "\u{1f4e6} Context at 91% \u{2014} compacting..."
        );
        assert_eq!(compaction_done(&started), None);
        assert_eq!(
            compaction_done(&compaction(CompactionStatus::Completed, Some(800))),
            Some((
                Tone::Info,
                "\u{2705} Compacted 12 messages (~800 token summary)".into()
            ))
        );
        assert_eq!(
            compaction_done(&compaction(CompactionStatus::Skipped, None))
                .unwrap()
                .1,
            "Not compacting: Conversation is too short"
        );
        assert_eq!(
            compaction_done(&compaction(CompactionStatus::Completed, None))
                .unwrap()
                .0,
            Tone::Warn
        );
    }

    #[test]
    fn a_plan_change_counts_added_changed_and_removed_steps() {
        let step = |s: &str, status| PlanStep {
            step: s.into(),
            status,
        };
        let before = [
            step("read", PlanStepStatus::InProgress),
            step("edit", PlanStepStatus::Pending),
        ];
        let after = [
            step("read", PlanStepStatus::Completed),
            step("test", PlanStepStatus::Pending),
        ];
        assert_eq!(plan_change(&before, &after), (1, 1, 1));
        assert_eq!(plan_change(&[], &after), (2, 0, 0));
    }
}
