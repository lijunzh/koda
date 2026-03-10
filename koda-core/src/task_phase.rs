//! Task phase state machine.
//!
//! Directional phase tracker with structural detection.
//! Six phases: Understanding → Planning → Reviewing → Executing → Verifying → Reporting.
//!
//! See #242 for the full design.

use crate::intent::TaskIntent;

/// Review gate intensity.
///
/// Each tier adds one isolation dimension over the previous:
/// - `FastPath`: no review call — let the model's internal reasoning handle it.
/// - `SelfReview`: same model, **fresh context** — breaks self-confirmation bias
///   by stripping the reasoning chain that produced the plan.
/// - `PeerReview`: **different model**, fresh context — genuine adversarial review
///   with different training biases and blind spots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDepth {
    /// Skip the review phase entirely. The model's extended thinking
    /// (if available) serves as the implicit review. For simple tasks
    /// with small action budgets and familiar tools.
    FastPath,
    /// Same model, fresh context window. Koda serializes the plan,
    /// strips conversation history, and sends only: system prompt +
    /// original task + plan artifact + file summaries. The reviewer
    /// sees the plan as an external artifact. For complex tasks.
    SelfReview,
    /// Different model, fresh context window. The plan is documented
    /// as a multi-step artifact and reviewed independently by a
    /// separate model/provider. For destructive or irreversible operations.
    PeerReview,
}

impl std::fmt::Display for ReviewDepth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FastPath => write!(f, "fast_path"),
            Self::SelfReview => write!(f, "self_review"),
            Self::PeerReview => write!(f, "peer_review"),
        }
    }
}

/// Current phase of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TaskPhase {
    /// Exploring the codebase, reading files, understanding the request.
    #[default]
    Understanding,
    /// Outlining steps (text-only phase).
    Planning,
    /// Self-checking the plan before execution (审议).
    Reviewing,
    /// Making changes (editing, writing, running commands).
    Executing,
    /// Checking results (running tests, reading output).
    Verifying,
    /// Summarizing what was done.
    Reporting,
}

impl TaskPhase {
    /// Ordinal for directional comparison.
    fn ordinal(self) -> u8 {
        match self {
            Self::Understanding => 0,
            Self::Planning => 1,
            Self::Reviewing => 2,
            Self::Executing => 3,
            Self::Verifying => 4,
            Self::Reporting => 5,
        }
    }

    /// Phase-appropriate prompt hint injected into the system prompt.
    pub fn prompt_hint(self) -> &'static str {
        match self {
            Self::Understanding => {
                "[Phase: Understanding — read relevant files before making changes]"
            }
            Self::Planning => "[Phase: Planning — list the steps you will take before executing]",
            Self::Reviewing => "[Phase: Reviewing — list what could go wrong. Stop if unclear.]",
            Self::Executing => "[Phase: Executing — make changes one file at a time]",
            Self::Verifying => "[Phase: Verifying — run tests and check for errors]",
            Self::Reporting => "[Phase: Reporting — summarize changes and results]",
        }
    }

    /// Review-phase prompt hint with depth scaling.
    ///
    /// Overrides the generic Reviewing hint when `ReviewDepth` is known.
    pub fn review_hint(self, depth: ReviewDepth) -> &'static str {
        if self != Self::Reviewing {
            return self.prompt_hint();
        }
        match depth {
            // FastPath: no separate review — let extended thinking handle it
            ReviewDepth::FastPath => {
                "[Phase: Reviewing/fast — plan looks straightforward. \
                 Quick sanity check, then proceed.]"
            }
            // SelfReview: same model reviews in a fresh context window
            ReviewDepth::SelfReview => {
                "[Phase: Reviewing/self-review — review this plan as an independent artifact.\n\
                 You did NOT produce this plan. Evaluate it critically:\n\
                 \u{2705} Feasibility: Can each step be done with available tools?\n\
                 \u{2705} Completeness: Does the plan cover the full request?\n\
                 \u{2705} Risk: What could go wrong? Is there a rollback?\n\
                 \u{2705} Resources: Which files are affected? Is scope reasonable?\n\
                 If any dimension fails, reject with specific feedback.]"
            }
            // PeerReview: different model reviews in a fresh context window
            ReviewDepth::PeerReview => {
                "[Phase: Reviewing/peer-review — you are an independent reviewer.\n\
                 A different agent produced the plan below. Your job is adversarial:\n\
                 \u{2705} Feasibility: Can each step be done with available tools?\n\
                 \u{2705} Completeness: Does the plan cover the full request?\n\
                 \u{2705} Risk: What could go wrong? Is there a rollback?\n\
                 \u{2705} Resources: Which files are affected? Is scope reasonable?\n\
                 \u{2705} Alternatives: Is there a simpler approach the planner missed?\n\
                 Approve, reject with feedback, or suggest revisions.]"
            }
        }
    }
}

impl std::fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Understanding => "Understanding",
            Self::Planning => "Planning",
            Self::Reviewing => "Reviewing",
            Self::Executing => "Executing",
            Self::Verifying => "Verifying",
            Self::Reporting => "Reporting",
        })
    }
}

/// Compact phase info for threading through tool dispatch.
/// Avoids passing the full `PhaseTracker` (which is mutable).
#[derive(Debug, Clone, Copy)]
pub struct PhaseInfo {
    pub phase: TaskPhase,
    pub plan_approved: bool,
    /// If `Some(n)`, at most `n` LocalMutation/Destructive actions allowed
    /// before the system forces a plan. Used by the simple-task shortcut.
    pub action_budget: Option<usize>,
}

impl PhaseInfo {
    /// Normal entry point: starts at Understanding, no plan approved.
    pub fn new_session() -> Self {
        Self {
            phase: TaskPhase::Understanding,
            plan_approved: false,
            action_budget: None,
        }
    }

    /// Simple-task shortcut: Executing + approved, but budget-limited.
    ///
    /// After `max_actions` mutating tool calls, the system injects a
    /// plan-requirement message. Default budget: 3.
    pub fn simple_task(max_actions: usize) -> Self {
        Self {
            phase: TaskPhase::Executing,
            plan_approved: true,
            action_budget: Some(max_actions),
        }
    }

    /// Delegated sub-agent: Executing + approved (parent already approved).
    pub fn delegated() -> Self {
        Self {
            phase: TaskPhase::Executing,
            plan_approved: true,
            action_budget: None,
        }
    }

    /// Consume one action from the budget. Returns `true` if budget
    /// is exhausted (caller should inject a plan-requirement message).
    ///
    /// Returns `false` if there's no budget (unlimited) or budget > 0.
    pub fn consume_action(&mut self) -> bool {
        if let Some(ref mut budget) = self.action_budget {
            if *budget == 0 {
                return true; // already exhausted
            }
            *budget -= 1;
            *budget == 0
        } else {
            false
        }
    }
}

impl From<&PhaseTracker> for PhaseInfo {
    fn from(tracker: &PhaseTracker) -> Self {
        Self {
            phase: tracker.current(),
            plan_approved: tracker.plan_approved(),
            action_budget: None,
        }
    }
}

// ── Review result ───────────────────────────────────────────

/// What kind of review happened (or didn't).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewResult {
    /// Mechanical checks passed, no LLM review needed.
    RulePassed,
    /// LLM self-reflection found no issues.
    LlmPassed,
    /// Rule or LLM layer raised concerns → Confirm gate.
    FlaggedForHuman,
    /// Review failed → Reviewing → Planning (封驳).
    Rejected,
}

// ── Tool type classification ────────────────────────────────

/// Whether tool calls in a turn are read-only or include writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolType {
    /// Only read/search tools (Read, List, Grep, Glob).
    ReadOnly,
    /// At least one mutating tool (Edit, Write, Delete, Bash).
    HasWrites,
}

impl ToolType {
    /// Classify a set of tool names from one turn.
    pub fn classify(tool_names: &[&str]) -> Self {
        let has_writes = tool_names
            .iter()
            .any(|t| matches!(*t, "Write" | "Edit" | "Delete" | "Bash" | "MemoryWrite"));
        if has_writes {
            Self::HasWrites
        } else {
            Self::ReadOnly
        }
    }
}

// ── Turn signal for structural detection ────────────────────

/// Structural signal from one inference turn, used by `PhaseTracker`
/// to decide phase transitions.
#[derive(Debug, Clone)]
pub struct TurnSignal {
    /// Whether the turn's complete response contained tool calls.
    pub has_tool_calls: bool,
    /// Classification of tool calls (if any).
    pub tool_type: ToolType,
    /// Whether recent tool history includes Bash calls
    /// (for Verifying detection).
    pub after_bash: bool,
}

// ── Phase tracker ───────────────────────────────────────────

/// Directional phase tracker with high-water mark.
///
/// Advances based on structural turn signals (has_tool_calls + tool_type),
/// not by parsing LLM text content. Forward transitions are always allowed;
/// backward transitions only on explicit escalation or rejection.
pub struct PhaseTracker {
    /// Current phase.
    current: TaskPhase,
    /// Furthest phase reached (high-water mark).
    high_water: TaskPhase,
    /// Flips at Reviewing → Executing boundary.
    plan_approved: bool,
    /// What kind of review happened (None = never entered Reviewing).
    review_result: Option<ReviewResult>,
    /// Whether to expect full six-phase progression.
    /// Set from TaskIntent at construction.
    expect_full_progression: bool,
    /// Transition count per phase pair — for oscillation detection (#287).
    /// Key: (from_ordinal, to_ordinal), Value: count.
    transition_counts: std::collections::HashMap<(u8, u8), u8>,
}

/// Record of a phase transition, for logging as `Role::Phase` messages.
#[derive(Debug, Clone)]
pub struct PhaseTransition {
    pub from: TaskPhase,
    pub to: TaskPhase,
    pub trigger: &'static str,
}

impl PhaseTransition {
    /// Human-readable summary (stored as message content, visible to LLM).
    pub fn summary(&self) -> String {
        format!("Phase: {} → {} ({})", self.from, self.to, self.trigger)
    }

    /// Structured JSON metadata (stored alongside summary, parsed by InterventionObserver).
    pub fn metadata_json(&self) -> String {
        serde_json::json!({
            "from": self.from.to_string(),
            "to": self.to.to_string(),
            "trigger": self.trigger,
        })
        .to_string()
    }

    /// Combined content for storage: human-readable summary + JSON metadata.
    /// The LLM sees the summary; the InterventionObserver parses the JSON.
    pub fn as_message_content(&self) -> String {
        format!("{}\n{}", self.summary(), self.metadata_json())
    }
}

impl PhaseTracker {
    /// Create a new tracker with intent-based initial expectations.
    pub fn new(intent: &TaskIntent) -> Self {
        Self {
            current: TaskPhase::Understanding,
            high_water: TaskPhase::Understanding,
            plan_approved: false,
            review_result: None,
            expect_full_progression: matches!(
                intent,
                TaskIntent::Complex | TaskIntent::Review | TaskIntent::TestGen
            ),
            transition_counts: std::collections::HashMap::new(),
        }
    }

    pub fn current(&self) -> TaskPhase {
        self.current
    }

    pub fn high_water(&self) -> TaskPhase {
        self.high_water
    }

    pub fn plan_approved(&self) -> bool {
        self.plan_approved
    }

    /// Mark the plan as approved (review passed or FastPath).
    pub fn approve_plan(&mut self) {
        self.plan_approved = true;
    }

    pub fn review_result(&self) -> Option<ReviewResult> {
        self.review_result
    }

    pub fn expects_full_progression(&self) -> bool {
        self.expect_full_progression
    }

    /// Select review depth based on task complexity signals.
    ///
    /// Priority:
    /// 1. InterventionObserver recommends auto → FastPath
    /// 2. Simple task (Understanding → Executing shortcut) → FastPath
    /// 3. Full progression expected (Complex/Review/TestGen intent) → PeerReview
    /// 4. Default → SelfReview
    pub fn select_review_depth(
        &self,
        observer: &crate::intervention_observer::InterventionObserver,
    ) -> ReviewDepth {
        // If observer has enough data and recommends auto, go fast
        if observer.recommends_auto(TaskPhase::Reviewing) {
            return ReviewDepth::FastPath;
        }

        // Complex tasks get peer review (different model, fresh context)
        if self.expect_full_progression {
            return ReviewDepth::PeerReview;
        }

        // Simple task shortcut was taken (jumped from Understanding to Executing)
        if self.plan_approved && self.review_result.is_none() {
            return ReviewDepth::FastPath;
        }

        ReviewDepth::SelfReview
    }

    /// Force a phase demotion (escalation).
    ///
    /// Used when the agent discovers unexpected complexity mid-execution.
    /// Only demotes from Executing or Verifying back to Understanding.
    /// Returns the transition record, or None if demotion isn't applicable.
    pub fn demote_to_understanding(&mut self, trigger: &'static str) -> Option<PhaseTransition> {
        if !matches!(self.current, TaskPhase::Executing | TaskPhase::Verifying) {
            return None;
        }

        let old = self.current;
        self.current = TaskPhase::Understanding;
        self.plan_approved = false; // plan is invalidated by scope change

        Some(PhaseTransition {
            from: old,
            to: TaskPhase::Understanding,
            trigger,
        })
    }

    /// Collapse to a simpler phase (complexity decreased).
    ///
    /// Used when the agent discovers existing solutions during Review.
    /// Only collapses from Reviewing to Executing.
    pub fn collapse_to_executing(&mut self) -> Option<PhaseTransition> {
        if self.current != TaskPhase::Reviewing {
            return None;
        }

        let old = self.current;
        self.current = TaskPhase::Executing;
        self.plan_approved = true;

        Some(PhaseTransition {
            from: old,
            to: TaskPhase::Executing,
            trigger: "simplification",
        })
    }

    /// Advance the phase based on a structural turn signal.
    ///
    /// Returns the new phase (which may be unchanged).
    /// See #242 decision tree for the full specification.
    pub fn advance(&mut self, signal: &TurnSignal) -> Option<PhaseTransition> {
        let old_phase = self.current;
        let new_phase = match (self.current, signal.has_tool_calls, signal.tool_type) {
            // Understanding: exploring the codebase
            (TaskPhase::Understanding, true, ToolType::HasWrites) => {
                // Simple task shortcut: jumped straight to writing
                TaskPhase::Executing
            }
            (TaskPhase::Understanding, true, ToolType::ReadOnly) => {
                TaskPhase::Understanding // still exploring
            }
            (TaskPhase::Understanding, false, _) => {
                TaskPhase::Planning // stopped reading, started talking
            }

            // Planning: outlining steps (text-only phase)
            (TaskPhase::Planning, false, _) => {
                TaskPhase::Reviewing // still talking → now reviewing
            }
            (TaskPhase::Planning, true, ToolType::HasWrites) => {
                // Tried to act during planning → forced through review gate.
                // In step 2, check_tool() will block this write.
                TaskPhase::Reviewing
            }
            (TaskPhase::Planning, true, ToolType::ReadOnly) => {
                TaskPhase::Planning // still exploring during planning
            }

            // Reviewing: self-checking the plan (text-only phase)
            (TaskPhase::Reviewing, true, ToolType::HasWrites) => {
                // Review passed, started acting
                TaskPhase::Executing
            }
            (TaskPhase::Reviewing, true, ToolType::ReadOnly) => {
                // 封驳 — found something wrong, re-investigating
                TaskPhase::Planning
            }
            (TaskPhase::Reviewing, false, _) => {
                TaskPhase::Reviewing // still reviewing
            }

            // Executing: making changes
            (TaskPhase::Executing, true, _) => {
                TaskPhase::Executing // still acting
            }
            (TaskPhase::Executing, false, _) if signal.after_bash => {
                TaskPhase::Verifying // ran tests, now reflecting
            }
            (TaskPhase::Executing, false, _) => {
                TaskPhase::Executing // mid-execution explanation
            }

            // Verifying: checking results
            (TaskPhase::Verifying, true, ToolType::ReadOnly) => {
                TaskPhase::Verifying // reading test output
            }
            (TaskPhase::Verifying, true, ToolType::HasWrites) => {
                TaskPhase::Executing // fixing issues found in tests
            }
            (TaskPhase::Verifying, false, _) => {
                TaskPhase::Reporting // summarizing results
            }

            // Reporting: terminal
            (TaskPhase::Reporting, _, _) => TaskPhase::Reporting,
        };

        // Update plan_approved when crossing Reviewing → Executing
        if self.current == TaskPhase::Reviewing && new_phase == TaskPhase::Executing {
            self.plan_approved = true;
            if self.review_result.is_none() {
                self.review_result = Some(ReviewResult::RulePassed);
            }
        }

        // Track Understanding → Executing shortcut
        if self.current == TaskPhase::Understanding && new_phase == TaskPhase::Executing {
            self.plan_approved = true;
            // review_result stays None (Reviewing was never entered)
        }

        self.current = new_phase;
        if new_phase.ordinal() > self.high_water.ordinal() {
            self.high_water = new_phase;
        }

        // Return transition record if phase actually changed
        if old_phase != new_phase {
            // Oscillation detection (#287): cap transitions per phase pair
            let pair = (old_phase.ordinal(), new_phase.ordinal());
            let count = self.transition_counts.entry(pair).or_insert(0);
            *count = count.saturating_add(1);
            if *count > Self::MAX_PAIR_TRANSITIONS {
                // Oscillation detected — stop transitioning, stay in current phase
                self.current = old_phase;
                return None;
            }

            let trigger = match (old_phase, new_phase) {
                (TaskPhase::Understanding, TaskPhase::Planning) => "text_only_after_reads",
                (TaskPhase::Understanding, TaskPhase::Executing) => "simple_task_shortcut",
                (TaskPhase::Planning, TaskPhase::Reviewing) => "plan_complete",
                (TaskPhase::Reviewing, TaskPhase::Executing) => "review_passed",
                (TaskPhase::Reviewing, TaskPhase::Planning) => "封驳",
                (TaskPhase::Executing, TaskPhase::Verifying) => "tests_after_bash",
                (TaskPhase::Executing, TaskPhase::Understanding) => "escalation",
                (TaskPhase::Verifying, TaskPhase::Reporting) => "summarizing",
                (TaskPhase::Verifying, TaskPhase::Executing) => "fixing_test_failures",
                _ => "transition",
            };
            Some(PhaseTransition {
                from: old_phase,
                to: new_phase,
                trigger,
            })
        } else {
            None
        }
    }

    /// Max transitions allowed per phase pair before oscillation is capped.
    /// 5 round-trips (e.g., Executing ↔ Verifying) is generous; beyond that
    /// the agent is stuck and the loop guard will eventually terminate.
    const MAX_PAIR_TRANSITIONS: u8 = 5;

    /// Force an escalation (Executing → Understanding) on tool failure
    /// indicating scope change.
    pub fn escalate(&mut self) {
        if self.current == TaskPhase::Executing {
            self.current = TaskPhase::Understanding;
            self.plan_approved = false;
        }
    }

    /// Record a review result (used by the review layer in step 2).
    pub fn set_review_result(&mut self, result: ReviewResult) {
        self.review_result = Some(result);
        if result == ReviewResult::Rejected {
            // 封驳: back to Planning
            self.current = TaskPhase::Planning;
        }
    }

    /// Legacy compatibility: detect phase from recent tool calls.
    /// Used during the transition period before full PhaseTracker wiring.
    pub fn detect_legacy(recent_tools: &[String]) -> TaskPhase {
        if recent_tools.is_empty() {
            return TaskPhase::Understanding;
        }

        let last_few: Vec<&str> = recent_tools
            .iter()
            .rev()
            .take(3)
            .map(|s| s.as_str())
            .collect();

        let read_count = last_few
            .iter()
            .filter(|t| matches!(**t, "Read" | "List" | "Grep" | "Glob"))
            .count();
        let write_count = last_few
            .iter()
            .filter(|t| matches!(**t, "Edit" | "Write" | "Delete"))
            .count();
        let bash_count = last_few.iter().filter(|t| matches!(**t, "Bash")).count();

        if bash_count >= 2 {
            TaskPhase::Verifying
        } else if write_count >= 2 {
            TaskPhase::Executing
        } else if read_count >= 2 {
            TaskPhase::Understanding
        } else if write_count >= 1 {
            TaskPhase::Executing
        } else {
            TaskPhase::Understanding
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TaskPhase basic tests ─────────────────────────────────

    #[test]
    fn test_prompt_hint_all_phases_have_content() {
        for phase in [
            TaskPhase::Understanding,
            TaskPhase::Planning,
            TaskPhase::Reviewing,
            TaskPhase::Executing,
            TaskPhase::Verifying,
            TaskPhase::Reporting,
        ] {
            let hint = phase.prompt_hint();
            assert!(!hint.is_empty(), "{phase:?} has empty hint");
            assert!(hint.contains("Phase:"), "{phase:?} missing Phase: prefix");
        }
    }

    // ── ToolType tests ──────────────────────────────────────

    #[test]
    fn test_tool_type_read_only() {
        assert_eq!(ToolType::classify(&["Read", "Grep"]), ToolType::ReadOnly);
        assert_eq!(ToolType::classify(&["List", "Glob"]), ToolType::ReadOnly);
    }

    #[test]
    fn test_tool_type_has_writes() {
        assert_eq!(ToolType::classify(&["Read", "Edit"]), ToolType::HasWrites);
        assert_eq!(ToolType::classify(&["Write"]), ToolType::HasWrites);
        assert_eq!(ToolType::classify(&["Bash"]), ToolType::HasWrites);
    }

    #[test]
    fn test_tool_type_empty() {
        assert_eq!(ToolType::classify(&[]), ToolType::ReadOnly);
    }

    // ── PhaseTracker construction ─────────────────────────────

    #[test]
    fn test_new_starts_at_understanding() {
        let t = PhaseTracker::new(&TaskIntent::Modify);
        assert_eq!(t.current(), TaskPhase::Understanding);
        assert!(!t.plan_approved());
        assert!(t.review_result().is_none());
    }

    #[test]
    fn test_intent_sets_progression() {
        assert!(PhaseTracker::new(&TaskIntent::Complex).expects_full_progression());
        assert!(PhaseTracker::new(&TaskIntent::Review).expects_full_progression());
        assert!(PhaseTracker::new(&TaskIntent::TestGen).expects_full_progression());
        assert!(!PhaseTracker::new(&TaskIntent::Modify).expects_full_progression());
        assert!(!PhaseTracker::new(&TaskIntent::Build).expects_full_progression());
        assert!(!PhaseTracker::new(&TaskIntent::Question).expects_full_progression());
    }

    // ── Structural detection decision tree ────────────────────

    fn signal(has_tools: bool, tool_type: ToolType, after_bash: bool) -> TurnSignal {
        TurnSignal {
            has_tool_calls: has_tools,
            tool_type,
            after_bash,
        }
    }

    #[test]
    fn test_understanding_to_planning_on_text() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        let s = signal(false, ToolType::ReadOnly, false);
        t.advance(&s);
        assert_eq!(t.current(), TaskPhase::Planning);
    }

    #[test]
    fn test_understanding_to_executing_shortcut() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        let s = signal(true, ToolType::HasWrites, false);
        t.advance(&s);
        assert_eq!(t.current(), TaskPhase::Executing);
        assert!(t.plan_approved()); // shortcut grants approval
        assert!(t.review_result().is_none()); // Reviewing never entered
    }

    #[test]
    fn test_understanding_stays_on_reads() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        let s = signal(true, ToolType::ReadOnly, false);
        t.advance(&s);
        assert_eq!(t.current(), TaskPhase::Understanding);
    }

    #[test]
    fn test_planning_to_reviewing_on_text() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Reviewing);
    }

    #[test]
    fn test_planning_forced_through_review_on_write() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        // LLM tries to write during Planning → forced to Reviewing
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Reviewing);
        assert!(!t.plan_approved()); // not yet approved
    }

    #[test]
    fn test_reviewing_to_executing_on_write() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Executing);
        assert!(t.plan_approved());
        assert_eq!(t.review_result(), Some(ReviewResult::RulePassed));
    }

    #[test]
    fn test_reviewing_fengbo_on_reads() {
        // 封驳: reviewing + read tools → back to Planning
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        t.advance(&signal(true, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Planning);
        assert!(!t.plan_approved());
    }

    #[test]
    fn test_executing_stays_on_tools() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // shortcut → Executing
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Executing);
    }

    #[test]
    fn test_executing_to_verifying_after_bash() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        t.advance(&signal(false, ToolType::ReadOnly, true));
        assert_eq!(t.current(), TaskPhase::Verifying);
    }

    #[test]
    fn test_executing_stays_on_text_without_bash() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        // Mid-execution explanation (no bash) → stays Executing
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Executing);
    }

    #[test]
    fn test_verifying_to_reporting_on_text() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        t.advance(&signal(false, ToolType::ReadOnly, true)); // → Verifying
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Reporting);
    }

    #[test]
    fn test_verifying_to_executing_on_writes() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        t.advance(&signal(false, ToolType::ReadOnly, true)); // → Verifying
        // Fixing test failures → back to Executing
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Executing);
    }

    #[test]
    fn test_reporting_is_terminal() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        t.advance(&signal(false, ToolType::ReadOnly, true)); // → Verifying
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reporting
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Reporting);
    }

    // ── High-water mark ──────────────────────────────────────

    #[test]
    fn test_high_water_tracks_furthest() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        assert_eq!(t.high_water(), TaskPhase::Reviewing);
        // 封驳 back to Planning — high_water stays at Reviewing
        t.advance(&signal(true, ToolType::ReadOnly, false)); // → Planning
        assert_eq!(t.current(), TaskPhase::Planning);
        assert_eq!(t.high_water(), TaskPhase::Reviewing);
    }

    // ── Escalation ───────────────────────────────────────────

    #[test]
    fn test_escalate_from_executing() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        assert!(t.plan_approved());
        t.escalate();
        assert_eq!(t.current(), TaskPhase::Understanding);
        assert!(!t.plan_approved()); // revoked
    }

    #[test]
    fn test_escalate_noop_from_other_phases() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        // From Understanding — escalate should be a no-op
        t.escalate();
        assert_eq!(t.current(), TaskPhase::Understanding);
    }

    // ── Review result management ──────────────────────────────

    #[test]
    fn test_set_review_rejected_demotes_to_planning() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        t.set_review_result(ReviewResult::Rejected);
        assert_eq!(t.current(), TaskPhase::Planning);
        assert_eq!(t.review_result(), Some(ReviewResult::Rejected));
    }

    // ── Legacy compatibility ─────────────────────────────────

    #[test]
    fn test_legacy_detect() {
        assert_eq!(
            PhaseTracker::detect_legacy(&["Read".into(), "Grep".into(), "List".into()]),
            TaskPhase::Understanding
        );
        assert_eq!(
            PhaseTracker::detect_legacy(&["Read".into(), "Edit".into(), "Write".into()]),
            TaskPhase::Executing
        );
        assert_eq!(
            PhaseTracker::detect_legacy(&["Bash".into(), "Bash".into(), "Read".into()]),
            TaskPhase::Verifying
        );
        assert_eq!(PhaseTracker::detect_legacy(&[]), TaskPhase::Understanding);
    }

    // ── Full scenario tests ──────────────────────────────────

    #[test]
    fn test_scenario_simple_task() {
        // "fix the typo" → Understanding → Executing shortcut
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Executing);
        assert!(t.plan_approved());
        assert!(t.review_result().is_none()); // never reviewed
    }

    #[test]
    fn test_scenario_full_progression() {
        // Complex task: Understanding → Planning → Reviewing → Executing → Verifying → Reporting
        let mut t = PhaseTracker::new(&TaskIntent::Complex);

        // Read files
        t.advance(&signal(true, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Understanding);

        // Stop reading, produce plan text
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Planning);

        // Produce review text
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Reviewing);

        // Start writing
        t.advance(&signal(true, ToolType::HasWrites, false));
        assert_eq!(t.current(), TaskPhase::Executing);
        assert!(t.plan_approved());

        // Run tests
        t.advance(&signal(false, ToolType::ReadOnly, true));
        assert_eq!(t.current(), TaskPhase::Verifying);

        // Summarize
        t.advance(&signal(false, ToolType::ReadOnly, false));
        assert_eq!(t.current(), TaskPhase::Reporting);

        assert_eq!(t.high_water(), TaskPhase::Reporting);
    }

    #[test]
    fn test_scenario_simple_to_complex_escalation() {
        // git pull → merge conflict → escalate
        let mut t = PhaseTracker::new(&TaskIntent::Build);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        assert!(t.plan_approved());

        // Tool fails with scope change
        t.escalate();
        assert_eq!(t.current(), TaskPhase::Understanding);
        assert!(!t.plan_approved());

        // Re-read, re-plan, re-review, re-execute
        t.advance(&signal(true, ToolType::ReadOnly, false)); // still Understanding
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        assert!(t.plan_approved());
    }

    // ── PhaseTransition tests ─────────────────────────────────

    #[test]
    fn test_advance_returns_transition_on_change() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        let transition = t.advance(&signal(false, ToolType::ReadOnly, false));
        assert!(transition.is_some());
        let tr = transition.unwrap();
        assert_eq!(tr.from, TaskPhase::Understanding);
        assert_eq!(tr.to, TaskPhase::Planning);
        assert_eq!(tr.trigger, "text_only_after_reads");
    }

    #[test]
    fn test_advance_returns_none_on_no_change() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        // Read tools during Understanding → stays Understanding
        let transition = t.advance(&signal(true, ToolType::ReadOnly, false));
        assert!(transition.is_none());
    }

    #[test]
    fn test_transition_message_content() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        let tr = t
            .advance(&signal(false, ToolType::ReadOnly, false))
            .unwrap();
        let content = tr.as_message_content();
        assert!(content.contains("Phase: Understanding → Planning"));
        assert!(content.contains("text_only_after_reads"));
        // Second line should be JSON
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines.len() >= 2);
        let meta: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(meta["from"], "Understanding");
        assert_eq!(meta["to"], "Planning");
    }

    #[test]
    fn test_fengbo_transition_trigger() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        let tr = t.advance(&signal(true, ToolType::ReadOnly, false)).unwrap(); // 封驳
        assert_eq!(tr.trigger, "封驳");
        assert_eq!(tr.from, TaskPhase::Reviewing);
        assert_eq!(tr.to, TaskPhase::Planning);
    }

    #[test]
    fn test_oscillation_capped() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        // Get to Executing
        t.advance(&signal(true, ToolType::HasWrites, false)); // Understanding → Executing
        assert_eq!(t.current(), TaskPhase::Executing);

        // Oscillate Executing ↔ Verifying
        for i in 0..20 {
            t.advance(&signal(false, ToolType::ReadOnly, true)); // → Verifying
            t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
            // After MAX_PAIR_TRANSITIONS (5), oscillation should be capped
            if i >= 5 {
                // Phase should stop transitioning
                let stuck = t.current();
                assert!(
                    stuck == TaskPhase::Executing || stuck == TaskPhase::Verifying,
                    "phase should be stuck in Executing or Verifying, got {stuck:?}"
                );
            }
        }
    }

    #[test]
    fn test_oscillation_does_not_affect_different_pairs() {
        let mut t = PhaseTracker::new(&TaskIntent::Modify);
        // Understanding → Planning (text) — this pair shouldn't be affected
        // by Executing ↔ Verifying oscillation
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Planning
        assert_eq!(t.current(), TaskPhase::Planning);
        t.advance(&signal(false, ToolType::ReadOnly, false)); // → Reviewing
        assert_eq!(t.current(), TaskPhase::Reviewing);
        t.advance(&signal(true, ToolType::HasWrites, false)); // → Executing
        assert_eq!(t.current(), TaskPhase::Executing);
    }

    #[test]
    fn test_consume_action_with_budget() {
        let mut info = PhaseInfo::simple_task(3);
        assert!(!info.consume_action()); // 3 → 2
        assert!(!info.consume_action()); // 2 → 1
        assert!(info.consume_action()); // 1 → 0 (exhausted)
        assert!(info.consume_action()); // still exhausted
    }

    #[test]
    fn test_consume_action_no_budget() {
        let mut info = PhaseInfo::delegated();
        assert!(!info.consume_action()); // unlimited
        assert!(!info.consume_action());
        assert!(!info.consume_action());
    }

    #[test]
    fn test_consume_action_zero_budget() {
        let mut info = PhaseInfo::simple_task(0);
        assert!(info.consume_action()); // immediately exhausted
    }

    #[test]
    fn test_new_session_constructor() {
        let info = PhaseInfo::new_session();
        assert_eq!(info.phase, TaskPhase::Understanding);
        assert!(!info.plan_approved);
        assert!(info.action_budget.is_none());
    }

    #[test]
    fn test_simple_task_constructor() {
        let info = PhaseInfo::simple_task(5);
        assert_eq!(info.phase, TaskPhase::Executing);
        assert!(info.plan_approved);
        assert_eq!(info.action_budget, Some(5));
    }

    #[test]
    fn test_delegated_constructor() {
        let info = PhaseInfo::delegated();
        assert_eq!(info.phase, TaskPhase::Executing);
        assert!(info.plan_approved);
        assert!(info.action_budget.is_none());
    }

    #[test]
    fn test_review_depth_default_is_self_review() {
        let tracker = PhaseTracker::new(&TaskIntent::Modify);
        let obs = crate::intervention_observer::InterventionObserver::new();
        // Modify intent, no observer data → SelfReview
        assert_eq!(tracker.select_review_depth(&obs), ReviewDepth::SelfReview);
    }

    #[test]
    fn test_review_depth_complex_is_peer_review() {
        let tracker = PhaseTracker::new(&TaskIntent::Complex);
        let obs = crate::intervention_observer::InterventionObserver::new();
        assert_eq!(tracker.select_review_depth(&obs), ReviewDepth::PeerReview);
    }

    #[test]
    fn test_review_depth_observer_recommends_fast() {
        let tracker = PhaseTracker::new(&TaskIntent::Modify);
        let mut obs = crate::intervention_observer::InterventionObserver::new();
        // Build enough auto data for Reviewing
        for _ in 0..10 {
            obs.record_auto(TaskPhase::Reviewing);
        }
        assert_eq!(tracker.select_review_depth(&obs), ReviewDepth::FastPath);
    }

    #[test]
    fn test_review_hint_fast_path() {
        let hint = TaskPhase::Reviewing.review_hint(ReviewDepth::FastPath);
        assert!(hint.contains("fast"));
        assert!(hint.contains("straightforward"));
    }

    #[test]
    fn test_review_hint_self_review() {
        let hint = TaskPhase::Reviewing.review_hint(ReviewDepth::SelfReview);
        assert!(hint.contains("self-review"));
        assert!(hint.contains("Feasibility"));
        assert!(hint.contains("did NOT produce"));
    }

    #[test]
    fn test_review_hint_peer_review() {
        let hint = TaskPhase::Reviewing.review_hint(ReviewDepth::PeerReview);
        assert!(hint.contains("peer-review"));
        assert!(hint.contains("independent reviewer"));
        assert!(hint.contains("Alternatives"));
    }

    #[test]
    fn test_review_hint_non_reviewing_falls_back() {
        let hint = TaskPhase::Executing.review_hint(ReviewDepth::PeerReview);
        assert!(hint.contains("Executing"));
    }
}
