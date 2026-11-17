//! A turn's token budget (`turn_token_budget`), as the peers cap a run.
//!
//! - **What counts** is Codex's rule for a goal's budget: input not read
//!   from the provider's cache, plus output, for every model call of the
//!   turn, sub-agents and compaction included. A long session isn't charged
//!   again for the history its cache serves.
//! - **Pacing:** a Claude model that takes the API's task budget is sent
//!   it, and sees it count down ([`crate::providers::TaskBudget`]). Any
//!   other model is told once, at 90% of the budget, to wrap up, in Codex's
//!   words for a goal that reached its budget.
//! - **The stop** is Claude Code's `--max-budget-usd`: checked as each model
//!   call returns, so the call that reaches the budget completes, and its
//!   tool calls run, and the turn makes no further call. Sub-agents stop at
//!   their next call, and a new one is refused. Spend can pass the budget by
//!   that one call and what running sub-agents spend before they stop.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::engine::UsageTotals;

/// Spend, as a share of the budget, at which a model that doesn't pace
/// itself is told to wrap up.
const WRAP_UP_PERCENT: u64 = 90;

/// One turn's budget and what it has spent.
#[derive(Debug)]
pub struct TurnBudget {
    limit: u64,
    spent: AtomicU64,
    wrap_up_told: AtomicBool,
    /// What was left when a compaction last replaced history, plus one;
    /// zero before any.
    left_at_compaction: AtomicU64,
}

impl TurnBudget {
    /// A budget of `limit` tokens, nothing spent.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            spent: AtomicU64::new(0),
            wrap_up_told: AtomicBool::new(false),
            left_at_compaction: AtomicU64::new(0),
        }
    }

    /// The budget.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Tokens charged so far.
    pub fn spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    /// Charge one model call.
    pub fn charge(&self, usage: &UsageTotals) {
        self.spent.fetch_add(charged(usage), Ordering::Relaxed);
    }

    /// Whether the turn has spent its budget.
    pub fn is_spent(&self) -> bool {
        self.spent() >= self.limit
    }

    /// What is left, never below zero.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.spent())
    }

    /// `true` once, the first time this is asked at or past
    /// 90% of the budget.
    pub fn take_wrap_up(&self) -> bool {
        self.spent().saturating_mul(100) >= self.limit.saturating_mul(WRAP_UP_PERCENT)
            && !self.wrap_up_told.swap(true, Ordering::Relaxed)
    }

    /// Note that a compaction replaced history: the server counted what it
    /// removed, and won't again.
    pub fn compacted(&self) {
        self.left_at_compaction
            .store(self.remaining() + 1, Ordering::Relaxed);
    }

    /// The task budget to send a Claude model that paces itself: the whole
    /// budget, with what was left at the last compaction as `remaining`,
    /// since the server no longer sees the history it counted. Changed only
    /// by a compaction, as the API asks, so the cached prefix holds between.
    pub fn task_budget(&self) -> crate::providers::TaskBudget {
        let left = self.left_at_compaction.load(Ordering::Relaxed);
        crate::providers::TaskBudget {
            total: self.limit,
            remaining: left.checked_sub(1),
        }
    }

    /// What the model is told when it should wrap up, from Codex's
    /// `budget_limit.md` for a goal that reached its budget.
    pub fn wrap_up_message(&self) -> String {
        format!(
            "System: This turn has used {} of its {}-token budget, and Koda stops it \
             once the budget is spent. Do not start new substantive work. Wrap up now: \
             summarize the useful progress, identify the remaining work or blockers, and \
             leave the user with a clear next step.",
            self.spent(),
            self.limit
        )
    }

    /// Why a stopped turn failed, shown after "Turn failed: ".
    pub fn stop_message(&self) -> String {
        format!(
            "token budget spent ({} of {} tokens, turn_token_budget); \
             send a follow-up to continue",
            self.spent(),
            self.limit
        )
    }
}

/// What one call costs against the budget: input not read from the cache,
/// plus output, as Codex charges a goal.
pub fn charged(usage: &UsageTotals) -> u64 {
    let uncached = usage.input_tokens().saturating_sub(usage.cache_read_tokens);
    (uncached.max(0) + usage.completion_tokens.max(0)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(prompt: i64, cache_read: i64, context: i64, completion: i64) -> UsageTotals {
        UsageTotals {
            prompt_tokens: prompt,
            cache_read_tokens: cache_read,
            context_tokens: context,
            completion_tokens: completion,
            api_calls: 1,
            ..Default::default()
        }
    }

    #[test]
    fn cached_input_is_not_charged() {
        // Anthropic: `prompt_tokens` leaves the cache out, `context_tokens`
        // has it all.
        assert_eq!(charged(&call(100, 9_000, 9_500, 50)), 550);
        // OpenAI-style: `prompt_tokens` counts the cached ones in.
        assert_eq!(charged(&call(9_500, 9_000, 0, 50)), 550);
        // A local server that reports no cache.
        assert_eq!(charged(&call(2_000, 0, 0, 300)), 2_300);
    }

    #[test]
    fn wrap_up_comes_once_at_ninety_percent_and_the_stop_at_the_budget() {
        let budget = TurnBudget::new(20_000);
        budget.charge(&call(17_000, 0, 0, 0));
        assert!(!budget.take_wrap_up());
        assert!(!budget.is_spent());
        budget.charge(&call(1_000, 0, 0, 0));
        assert!(budget.take_wrap_up(), "18,000 is 90%");
        assert!(!budget.take_wrap_up(), "told once");
        budget.charge(&call(2_500, 0, 0, 0));
        assert!(budget.is_spent());
        assert_eq!(budget.remaining(), 0);
        assert!(budget.stop_message().contains("20500 of 20000 tokens"));
    }

    #[test]
    fn the_task_budget_carries_what_was_left_at_a_compaction() {
        let budget = TurnBudget::new(50_000);
        budget.charge(&call(5_000, 0, 0, 0));
        assert_eq!(budget.task_budget().remaining, None, "the server counts");
        budget.compacted();
        budget.charge(&call(3_000, 0, 0, 0));
        // Fixed at the compaction, not lowered on every call.
        assert_eq!(budget.task_budget().remaining, Some(45_000));
        assert_eq!(budget.task_budget().total, 50_000);
    }
}
