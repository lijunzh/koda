//! Runtime tier adaptation based on observed model behavior.
//!
//! Instead of guessing model capability from names, we start all models
//! at Standard tier and promote/demote based on actual tool-use quality.
//!
//! Promotion (Standard → Strong): model uses tools correctly for N turns.
//! Demotion  (Standard → Lite):   model hallucinates tool names or sends
//!                                malformed JSON.
//!
//! Tier changes are only applied at compaction boundaries (when the
//! system prompt is rebuilt anyway) to preserve prompt-cache hit rates.

use crate::model_tier::ModelTier;

/// Number of turns to observe before considering promotion.
const PROMOTION_THRESHOLD: u32 = 3;
/// Number of failures before demotion.
const DEMOTION_THRESHOLD: u32 = 2;

/// Tracks tool-use quality signals across turns.
#[derive(Debug, Clone)]
pub struct TierObserver {
    /// Number of valid tool calls (known name + parseable JSON).
    valid_calls: u32,
    /// Number of tool calls with unknown/hallucinated names.
    hallucinated_names: u32,
    /// Number of tool calls with malformed JSON arguments.
    malformed_args: u32,
    /// Number of turns observed (turns with at least one tool call).
    turns_with_tools: u32,
    /// Whether the tier was explicitly set (CLI flag / agent config).
    /// If true, the observer never overrides it.
    explicitly_set: bool,
    /// Current effective tier.
    current: ModelTier,
    /// Whether a tier transition has been recommended but not yet applied.
    pending_transition: Option<ModelTier>,
}

impl TierObserver {
    /// Create a new observer.
    ///
    /// `initial` — starting tier (Standard unless explicitly overridden).
    /// `explicitly_set` — if true, the observer will never recommend changes.
    pub fn new(initial: ModelTier, explicitly_set: bool) -> Self {
        Self {
            valid_calls: 0,
            hallucinated_names: 0,
            malformed_args: 0,
            turns_with_tools: 0,
            explicitly_set,
            current: initial,
            pending_transition: None,
        }
    }

    /// Record the outcome of a single tool call.
    pub fn record_tool_call(&mut self, outcome: ToolCallOutcome) {
        match outcome {
            ToolCallOutcome::Valid => self.valid_calls += 1,
            ToolCallOutcome::UnknownTool => self.hallucinated_names += 1,
            ToolCallOutcome::MalformedArgs => self.malformed_args += 1,
        }
    }

    /// Mark end of a turn that had tool calls.
    /// Updates the recommended tier based on accumulated signals.
    pub fn end_turn(&mut self) {
        self.turns_with_tools += 1;
        self.evaluate();
    }

    /// Get the current effective tier.
    pub fn current_tier(&self) -> ModelTier {
        self.current
    }

    /// Consume any pending tier transition.
    ///
    /// Call this at compaction boundaries to apply the transition.
    /// Returns `Some(new_tier)` if a transition is pending.
    pub fn take_pending_transition(&mut self) -> Option<ModelTier> {
        if self.explicitly_set {
            return None;
        }
        self.pending_transition.take()
    }

    /// Evaluate signals and recommend promotion/demotion.
    fn evaluate(&mut self) {
        if self.explicitly_set {
            return;
        }

        // Demotion: too many failures → Lite
        if self.hallucinated_names >= DEMOTION_THRESHOLD
            || self.malformed_args >= DEMOTION_THRESHOLD
        {
            if self.current != ModelTier::Lite {
                tracing::info!(
                    "TierObserver: demoting to Lite \
                     (hallucinated={}, malformed={})",
                    self.hallucinated_names,
                    self.malformed_args
                );
                self.pending_transition = Some(ModelTier::Lite);
                self.current = ModelTier::Lite;
            }
            return;
        }

        // Promotion: consistent success → Strong
        if self.turns_with_tools >= PROMOTION_THRESHOLD
            && self.valid_calls > 0
            && self.hallucinated_names == 0
            && self.malformed_args == 0
            && self.current != ModelTier::Strong
        {
            tracing::info!(
                "TierObserver: promoting to Strong \
                 (valid={}, turns={})",
                self.valid_calls,
                self.turns_with_tools
            );
            self.pending_transition = Some(ModelTier::Strong);
            self.current = ModelTier::Strong;
        }
    }
}

/// Outcome of a single tool call, as observed by the inference loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallOutcome {
    /// Tool name was known and arguments parsed successfully.
    Valid,
    /// Tool name was not in the registry (hallucinated).
    UnknownTool,
    /// Tool name was known but arguments were malformed JSON.
    MalformedArgs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_given_tier() {
        let obs = TierObserver::new(ModelTier::Standard, false);
        assert_eq!(obs.current_tier(), ModelTier::Standard);
    }

    #[test]
    fn promotes_after_threshold() {
        let mut obs = TierObserver::new(ModelTier::Standard, false);
        for _ in 0..3 {
            obs.record_tool_call(ToolCallOutcome::Valid);
            obs.end_turn();
        }
        assert_eq!(obs.current_tier(), ModelTier::Strong);
        assert_eq!(obs.take_pending_transition(), Some(ModelTier::Strong));
    }

    #[test]
    fn demotes_on_hallucinations() {
        let mut obs = TierObserver::new(ModelTier::Standard, false);
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.end_turn();
        assert_eq!(obs.current_tier(), ModelTier::Lite);
    }

    #[test]
    fn demotes_on_malformed_args() {
        let mut obs = TierObserver::new(ModelTier::Standard, false);
        obs.record_tool_call(ToolCallOutcome::MalformedArgs);
        obs.record_tool_call(ToolCallOutcome::MalformedArgs);
        obs.end_turn();
        assert_eq!(obs.current_tier(), ModelTier::Lite);
    }

    #[test]
    fn explicit_set_blocks_transitions() {
        let mut obs = TierObserver::new(ModelTier::Standard, true);
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.end_turn();
        assert_eq!(obs.current_tier(), ModelTier::Standard);
        assert_eq!(obs.take_pending_transition(), None);
    }

    #[test]
    fn no_promotion_with_failures() {
        let mut obs = TierObserver::new(ModelTier::Standard, false);
        for _ in 0..3 {
            obs.record_tool_call(ToolCallOutcome::Valid);
            obs.end_turn();
        }
        // Now add a hallucination — shouldn't stay Strong
        // (already promoted, but adding hallucinations demotes)
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.record_tool_call(ToolCallOutcome::UnknownTool);
        obs.end_turn();
        assert_eq!(obs.current_tier(), ModelTier::Lite);
    }

    #[test]
    fn pending_transition_consumed_once() {
        let mut obs = TierObserver::new(ModelTier::Standard, false);
        for _ in 0..3 {
            obs.record_tool_call(ToolCallOutcome::Valid);
            obs.end_turn();
        }
        assert_eq!(obs.take_pending_transition(), Some(ModelTier::Strong));
        // Second take returns None
        assert_eq!(obs.take_pending_transition(), None);
    }
}
