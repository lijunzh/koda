//! Plan review system: typed handoffs between Planning and Reviewing phases.
//!
//! Two tools form the phase transition contracts:
//! - `SubmitPlan`: planner's exit — crystallizes reasoning into a structured artifact
//! - `SubmitReview`: reviewer's exit — typed verdict on the plan
//!
//! See #335 for the full design.

use crate::providers::{LlmProvider, ToolDefinition};
use crate::task_phase::ReviewDepth;
use crate::tools::ToolEffect;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ── Plan artifact (from submit_plan tool) ───────────────────

/// A single step in a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    pub tool: String,
    pub files: Vec<String>,
    pub effect: ToolEffect,
}

/// The planner's crystallized output — typed from birth via `submit_plan` tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanArtifact {
    pub goal: String,
    pub steps: Vec<PlanStep>,
}

impl PlanArtifact {
    /// Parse from the submit_plan tool call arguments.
    pub fn from_tool_args(args: &serde_json::Value) -> Result<Self, String> {
        let goal = args["goal"]
            .as_str()
            .ok_or("missing 'goal' field")?
            .to_string();

        let steps_val = args["steps"]
            .as_array()
            .ok_or("missing or invalid 'steps' array")?;

        if steps_val.is_empty() {
            return Err("plan must have at least one step".to_string());
        }

        let mut steps = Vec::with_capacity(steps_val.len());
        for (i, s) in steps_val.iter().enumerate() {
            steps.push(PlanStep {
                description: s["description"]
                    .as_str()
                    .ok_or(format!("step {i}: missing 'description'"))?
                    .to_string(),
                tool: s["tool"]
                    .as_str()
                    .ok_or(format!("step {i}: missing 'tool'"))?
                    .to_string(),
                files: s["files"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                effect: serde_json::from_value(s["effect"].clone())
                    .unwrap_or(ToolEffect::LocalMutation),
            });
        }

        Ok(Self { goal, steps })
    }

    /// Render as markdown for the reviewer's context window.
    pub fn to_review_markdown(&self, task: &str) -> String {
        let mut md = format!("## Task\n{}\n\n## Goal\n{}\n\n## Plan\n", task, self.goal);
        for (i, step) in self.steps.iter().enumerate() {
            let files = if step.files.is_empty() {
                String::new()
            } else {
                format!(", files: [{}]", step.files.join(", "))
            };
            md.push_str(&format!(
                "{}. {} — tool: {}{}, effect: {:?}\n",
                i + 1,
                step.description,
                step.tool,
                files,
                step.effect,
            ));
        }
        md
    }

    /// Check if any step has a destructive effect.
    pub fn has_destructive_step(&self) -> bool {
        self.steps
            .iter()
            .any(|s| s.effect == ToolEffect::Destructive)
    }

    /// Check if any step has a remote action effect.
    pub fn has_remote_step(&self) -> bool {
        self.steps
            .iter()
            .any(|s| s.effect == ToolEffect::RemoteAction)
    }

    /// Affected file paths (deduplicated), capped at `limit`.
    pub fn affected_files(&self, limit: usize) -> (Vec<String>, usize) {
        let mut seen = std::collections::HashSet::new();
        let mut files = Vec::new();
        for step in &self.steps {
            for f in &step.files {
                if seen.insert(f.clone()) {
                    files.push(f.clone());
                }
            }
        }
        let total = files.len();
        files.truncate(limit);
        (files, total)
    }
}

// ── Review verdict (from submit_review tool) ────────────────

/// Reviewer's typed verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    Rejected,
    Revised,
}

impl std::fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
            Self::Revised => write!(f, "revised"),
        }
    }
}

/// Human's arbitration decision (for PeerReview disagreements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanDecision {
    AcceptedPlan,
    AcceptedReview,
    ManualEdit,
    Aborted,
}

impl std::fmt::Display for HumanDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AcceptedPlan => write!(f, "accepted_plan"),
            Self::AcceptedReview => write!(f, "accepted_review"),
            Self::ManualEdit => write!(f, "manual_edit"),
            Self::Aborted => write!(f, "aborted"),
        }
    }
}

/// Why the review gate was engaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateReason {
    /// `ToolEffect::Destructive` detected in plan → PeerReview.
    DestructiveFloor,
    /// `ToolEffect::RemoteAction` detected in plan → at least SelfReview.
    RemoteActionFloor,
    /// Plan has >3 steps or full progression expected.
    ComplexityThreshold,
    /// `InterventionObserver` recommended auto.
    ObserverAuto,
    /// PeerReview rejected, escalated to human.
    PeerReviewDisagreement,
    /// SelfReview re-plan budget (2) exceeded.
    RePlanExhausted,
}

impl std::fmt::Display for GateReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DestructiveFloor => write!(f, "destructive_floor"),
            Self::RemoteActionFloor => write!(f, "remote_action_floor"),
            Self::ComplexityThreshold => write!(f, "complexity_threshold"),
            Self::ObserverAuto => write!(f, "observer_auto"),
            Self::PeerReviewDisagreement => write!(f, "peer_review_disagreement"),
            Self::RePlanExhausted => write!(f, "re_plan_exhausted"),
        }
    }
}

/// Result of the submit_review tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitReviewResult {
    pub verdict: ReviewVerdict,
    pub reasoning: String,
    pub suggested_changes: Option<Vec<String>>,
}

impl SubmitReviewResult {
    /// Parse from the submit_review tool call arguments.
    pub fn from_tool_args(args: &serde_json::Value) -> Result<Self, String> {
        let verdict_str = args["verdict"].as_str().ok_or("missing 'verdict' field")?;
        let verdict: ReviewVerdict = serde_json::from_value(json!(verdict_str)).map_err(|_| {
            format!("invalid verdict: '{verdict_str}'. Expected: approved, rejected, revised")
        })?;

        let reasoning = args["reasoning"].as_str().unwrap_or("").to_string();

        let suggested_changes = args["suggested_changes"].as_array().map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });

        Ok(Self {
            verdict,
            reasoning,
            suggested_changes,
        })
    }
}

// ── Review record (for DB persistence) ──────────────────────

/// A review event persisted to the `review_records` table.
/// Only created for SelfReview and PeerReview (not FastPath).
pub struct ReviewRecord {
    pub review_depth: ReviewDepth,
    pub reviewer_model: String,
    pub planner_model: String,
    pub plan_summary: String,
    pub reviewer_verdict: ReviewVerdict,
    pub reviewer_reasoning: Option<String>,
    pub human_decision: Option<HumanDecision>,
    pub gate_reason: GateReason,
}

impl ReviewRecord {
    /// Outcome is derived, not stored.
    /// When human was asked: outcome = human_decision.
    /// When human was not asked and reviewer approved: outcome = AcceptedPlan.
    pub fn outcome(&self) -> HumanDecision {
        self.human_decision.unwrap_or(HumanDecision::AcceptedPlan)
    }
}

// ── Tool definitions ────────────────────────────────────────

/// Tool definition for `SubmitPlan`.
pub fn submit_plan_definition() -> ToolDefinition {
    ToolDefinition {
        name: "SubmitPlan".to_string(),
        description: "Submit a structured plan for review. Call this when you have finished \
            planning and want to proceed to execution. The plan will be reviewed before \
            any changes are made."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "Your one-line interpretation of what the task requires"
                },
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {
                                "type": "string",
                                "description": "What this step does"
                            },
                            "tool": {
                                "type": "string",
                                "description": "Which tool will be used (Read, Edit, Bash, etc.)"
                            },
                            "files": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Files affected by this step"
                            },
                            "effect": {
                                "type": "string",
                                "enum": ["ReadOnly", "RemoteAction", "LocalMutation", "Destructive"],
                                "description": "Effect classification of this step"
                            }
                        },
                        "required": ["description", "tool"]
                    },
                    "description": "Ordered list of steps to execute"
                }
            },
            "required": ["goal", "steps"]
        }),
    }
}

/// Tool definition for `SubmitReview`.
pub fn submit_review_definition() -> ToolDefinition {
    ToolDefinition {
        name: "SubmitReview".to_string(),
        description: "Submit your review verdict on the plan. You MUST call this tool \
            to complete your review."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "verdict": {
                    "type": "string",
                    "enum": ["approved", "rejected", "revised"],
                    "description": "Your verdict: approved (plan is sound), \
                        rejected (plan has issues, needs re-planning), or \
                        revised (plan is mostly good but needs specific changes)"
                },
                "reasoning": {
                    "type": "string",
                    "description": "Why you reached this verdict"
                },
                "suggested_changes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Specific changes to make (for rejected/revised verdicts)"
                }
            },
            "required": ["verdict", "reasoning"]
        }),
    }
}

// ── PeerReview model resolution ──────────────────────────────

use crate::config::ProviderType;

/// Ranked preference lists per provider family.
/// Ordered by preference; fallthrough on model-not-found.
const GEMINI_REVIEWER_MODELS: &[&str] = &[
    "gemini-3.1-pro-preview-customtools", // best for structured tool use
    "gemini-2.5-pro",                     // stable fallback
    "gemini-2.5-flash",                   // cost fallback
];

const ANTHROPIC_REVIEWER_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5-20250514",
    "claude-haiku-4-5-20251001",
];

const OPENAI_REVIEWER_MODELS: &[&str] = &["gpt-5.4", "gpt-5.2", "o3"];

/// Resolve which provider+model to use for PeerReview.
///
/// Tries a prioritized fallback chain based on the planner's provider:
/// - Anthropic planner → Gemini, then OpenAI, then SelfReview
/// - Gemini planner → Anthropic, then OpenAI, then SelfReview
/// - OpenAI planner → Anthropic, then Gemini, then SelfReview
/// - Other planner → Gemini, then Anthropic, then OpenAI, then SelfReview
///
/// Returns `None` if no cross-provider key is available (caller downgrades to SelfReview).
pub fn resolve_reviewer_provider(
    planner_provider: &ProviderType,
) -> Option<(ProviderType, &'static [&'static str])> {
    // KODA_REVIEWER_MODEL override skips resolution
    if let Ok(model) = std::env::var("KODA_REVIEWER_MODEL")
        && !model.is_empty()
    {
        return None; // Caller handles env var override separately
    }

    // Fallback chain: ordered list of (provider, models, key_names) to try
    let chain: &[(ProviderType, &[&str], &[&str])] = match planner_provider {
        ProviderType::Anthropic => &[
            (
                ProviderType::Gemini,
                GEMINI_REVIEWER_MODELS,
                &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ),
            (
                ProviderType::OpenAI,
                OPENAI_REVIEWER_MODELS,
                &["OPENAI_API_KEY"],
            ),
        ],
        ProviderType::Gemini => &[
            (
                ProviderType::Anthropic,
                ANTHROPIC_REVIEWER_MODELS,
                &["ANTHROPIC_API_KEY"],
            ),
            (
                ProviderType::OpenAI,
                OPENAI_REVIEWER_MODELS,
                &["OPENAI_API_KEY"],
            ),
        ],
        ProviderType::OpenAI => &[
            (
                ProviderType::Anthropic,
                ANTHROPIC_REVIEWER_MODELS,
                &["ANTHROPIC_API_KEY"],
            ),
            (
                ProviderType::Gemini,
                GEMINI_REVIEWER_MODELS,
                &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ),
        ],
        // Local/other providers: try all three
        _ => &[
            (
                ProviderType::Gemini,
                GEMINI_REVIEWER_MODELS,
                &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ),
            (
                ProviderType::Anthropic,
                ANTHROPIC_REVIEWER_MODELS,
                &["ANTHROPIC_API_KEY"],
            ),
            (
                ProviderType::OpenAI,
                OPENAI_REVIEWER_MODELS,
                &["OPENAI_API_KEY"],
            ),
        ],
    };

    // Try each provider in the chain; first with an available key wins
    for (provider_type, models, key_names) in chain {
        if key_names.iter().any(|k| std::env::var(k).is_ok()) {
            return Some((provider_type.clone(), models));
        }
    }

    None // No cross-provider key available → SelfReview
}

/// Create a provider instance for the reviewer.
pub fn create_reviewer_provider(
    reviewer_type: &ProviderType,
    _model: &str,
) -> Box<dyn LlmProvider> {
    let base_url = reviewer_type.meta().url.to_string();
    let env_key = reviewer_type.env_key_name();
    let api_key = std::env::var(env_key).ok().or_else(|| {
        // Try alternate key for Gemini
        if matches!(reviewer_type, ProviderType::Gemini) {
            std::env::var("GOOGLE_API_KEY").ok()
        } else {
            None
        }
    });

    match reviewer_type {
        ProviderType::Anthropic => Box::new(crate::providers::anthropic::AnthropicProvider::new(
            api_key.unwrap_or_default(),
            Some(&base_url),
        )),
        ProviderType::Gemini => Box::new(crate::providers::gemini::GeminiProvider::new(
            api_key.unwrap_or_default(),
            Some(&base_url),
        )),
        _ => Box::new(crate::providers::openai_compat::OpenAiCompatProvider::new(
            &base_url, api_key,
        )),
    }
}

// ── run_review: standalone review function ──────────────────

/// Build the reviewer's system prompt.
fn reviewer_system_prompt(depth: ReviewDepth) -> &'static str {
    match depth {
        ReviewDepth::SelfReview => {
            "You are an independent code reviewer. You did NOT produce the plan below. \
             Evaluate it critically using these dimensions:\n\
             1. Feasibility: Can each step be done with available tools?\n\
             2. Completeness: Does the plan cover the full request?\n\
             3. Risk: What could go wrong? Is there a rollback?\n\
             4. Resources: Which files are affected? Is scope reasonable?\n\n\
             You MUST call the SubmitReview tool with your verdict."
        }
        ReviewDepth::PeerReview => {
            "You are an independent reviewer. A DIFFERENT agent produced the plan below. \
             Your job is adversarial — find what the planner missed:\n\
             1. Feasibility: Can each step be done with available tools?\n\
             2. Completeness: Does the plan cover the full request?\n\
             3. Risk: What could go wrong? Is there a rollback?\n\
             4. Resources: Which files are affected? Is scope reasonable?\n\
             5. Alternatives: Is there a simpler approach the planner missed?\n\n\
             You MUST call the SubmitReview tool with your verdict."
        }
        ReviewDepth::FastPath => {
            unreachable!("FastPath should never call run_review")
        }
    }
}

/// Run a review of the plan artifact.
///
/// For SelfReview: same provider as the planner, fresh context.
/// For PeerReview: different provider, fresh context.
/// For FastPath: this function is never called.
///
/// Returns the reviewer's typed verdict.
pub async fn run_review(
    plan: &PlanArtifact,
    task: &str,
    provider: &dyn LlmProvider,
    depth: ReviewDepth,
    reviewer_model: &str,
) -> anyhow::Result<SubmitReviewResult> {
    use crate::providers::ChatMessage;

    let system = reviewer_system_prompt(depth).to_string();

    // Build the plan markdown for the reviewer
    let plan_md = plan.to_review_markdown(task);

    // Affected files summary
    let (files, total) = plan.affected_files(20);
    let files_section = if files.is_empty() {
        String::new()
    } else {
        let mut s = "\n## Affected Files\n".to_string();
        for f in &files {
            s.push_str(&format!("- {f}\n"));
        }
        if total > files.len() {
            s.push_str(&format!("...and {} other files\n", total - files.len()));
        }
        s
    };

    let user_content = format!("{plan_md}{files_section}");

    let messages = vec![
        ChatMessage::text("system", &system),
        ChatMessage::text("user", &user_content),
    ];

    // Only tool available to the reviewer
    let tools = vec![submit_review_definition()];

    let settings = crate::config::ModelSettings {
        model: reviewer_model.to_string(),
        max_tokens: Some(2048),
        temperature: Some(0.0),
        thinking_budget: None,
        reasoning_effort: None,
        max_context_tokens: 32_000,
    };
    let response = provider.chat(&messages, &tools, &settings).await?;

    // Extract the SubmitReview tool call from the response
    for tc in &response.tool_calls {
        if tc.function_name == "SubmitReview" {
            let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                .map_err(|e| anyhow::anyhow!("Failed to parse SubmitReview args: {e}"))?;
            return SubmitReviewResult::from_tool_args(&args)
                .map_err(|e| anyhow::anyhow!("Invalid SubmitReview: {e}"));
        }
    }

    // If reviewer didn't call the tool, treat as rejection (safe default)
    Ok(SubmitReviewResult {
        verdict: ReviewVerdict::Rejected,
        reasoning: "Reviewer did not submit a structured verdict. \
            Treating as rejection for safety."
            .to_string(),
        suggested_changes: None,
    })
}

/// Run PeerReview with model fallthrough.
///
/// Tries models from the preference list. On model-not-found errors,
/// falls through to the next. Caches the resolved model name.
/// Returns `None` if all models fail or no cross-provider key available
/// (caller should downgrade to SelfReview).
pub async fn run_peer_review(
    plan: &PlanArtifact,
    task: &str,
    planner_provider: &ProviderType,
    cached_reviewer: &mut Option<(ProviderType, String)>,
) -> Option<anyhow::Result<SubmitReviewResult>> {
    // Check for KODA_REVIEWER_MODEL override
    if let Ok(model) = std::env::var("KODA_REVIEWER_MODEL")
        && !model.is_empty()
    {
        let provider_type = if model.starts_with("claude") {
            ProviderType::Anthropic
        } else if model.starts_with("gemini") {
            ProviderType::Gemini
        } else {
            ProviderType::OpenAI
        };
        let provider = create_reviewer_provider(&provider_type, &model);
        return Some(
            run_review(
                plan,
                task,
                provider.as_ref(),
                ReviewDepth::PeerReview,
                &model,
            )
            .await,
        );
    }

    // Use cached reviewer if available
    if let Some((ptype, model)) = cached_reviewer.as_ref() {
        let provider = create_reviewer_provider(ptype, model);
        return Some(
            run_review(
                plan,
                task,
                provider.as_ref(),
                ReviewDepth::PeerReview,
                model,
            )
            .await,
        );
    }

    // Resolve reviewer from preference list
    let (reviewer_type, models) = resolve_reviewer_provider(planner_provider)?;

    for &model in models {
        let provider = create_reviewer_provider(&reviewer_type, model);
        let result = run_review(
            plan,
            task,
            provider.as_ref(),
            ReviewDepth::PeerReview,
            model,
        )
        .await;

        match &result {
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                // Model-not-found heuristic: 404, "model not found", "model_not_found"
                if msg.contains("404")
                    || msg.contains("model not found")
                    || msg.contains("model_not_found")
                    || msg.contains("not found")
                {
                    tracing::info!("Reviewer model {model} not available, trying next");
                    continue;
                }
                // Other error (rate limit, auth, etc.) — return it
                return Some(result);
            }
            Ok(_) => {
                // Cache the resolved model for the rest of the session
                *cached_reviewer = Some((reviewer_type.clone(), model.to_string()));
                return Some(result);
            }
        }
    }

    // All models failed — caller should downgrade to SelfReview
    None
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_artifact_from_valid_args() {
        let args = json!({
            "goal": "Fix the bug in auth",
            "steps": [
                {
                    "description": "Read the auth module",
                    "tool": "Read",
                    "files": ["src/auth.rs"],
                    "effect": "ReadOnly"
                },
                {
                    "description": "Edit the validation logic",
                    "tool": "Edit",
                    "files": ["src/auth.rs"],
                    "effect": "LocalMutation"
                }
            ]
        });
        let plan = PlanArtifact::from_tool_args(&args).unwrap();
        assert_eq!(plan.goal, "Fix the bug in auth");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].tool, "Read");
        assert_eq!(plan.steps[1].effect, ToolEffect::LocalMutation);
    }

    #[test]
    fn test_plan_artifact_missing_goal() {
        let args = json!({ "steps": [{ "description": "x", "tool": "Read" }] });
        assert!(PlanArtifact::from_tool_args(&args).is_err());
    }

    #[test]
    fn test_plan_artifact_empty_steps() {
        let args = json!({ "goal": "test", "steps": [] });
        let err = PlanArtifact::from_tool_args(&args).unwrap_err();
        assert!(err.contains("at least one step"));
    }

    #[test]
    fn test_plan_artifact_missing_step_fields() {
        let args = json!({ "goal": "test", "steps": [{ "description": "x" }] });
        let err = PlanArtifact::from_tool_args(&args).unwrap_err();
        assert!(err.contains("missing 'tool'"));
    }

    #[test]
    fn test_plan_has_destructive_step() {
        let plan = PlanArtifact {
            goal: "delete stuff".into(),
            steps: vec![
                PlanStep {
                    description: "read".into(),
                    tool: "Read".into(),
                    files: vec![],
                    effect: ToolEffect::ReadOnly,
                },
                PlanStep {
                    description: "delete".into(),
                    tool: "Bash".into(),
                    files: vec!["data/".into()],
                    effect: ToolEffect::Destructive,
                },
            ],
        };
        assert!(plan.has_destructive_step());
        assert!(!plan.has_remote_step());
    }

    #[test]
    fn test_affected_files_dedup_and_cap() {
        let plan = PlanArtifact {
            goal: "test".into(),
            steps: vec![
                PlanStep {
                    description: "a".into(),
                    tool: "Edit".into(),
                    files: vec!["a.rs".into(), "b.rs".into()],
                    effect: ToolEffect::LocalMutation,
                },
                PlanStep {
                    description: "b".into(),
                    tool: "Edit".into(),
                    files: vec!["b.rs".into(), "c.rs".into()],
                    effect: ToolEffect::LocalMutation,
                },
            ],
        };
        let (files, total) = plan.affected_files(2);
        assert_eq!(files.len(), 2);
        assert_eq!(total, 3);
    }

    #[test]
    fn test_to_review_markdown() {
        let plan = PlanArtifact {
            goal: "Fix auth".into(),
            steps: vec![PlanStep {
                description: "Edit auth module".into(),
                tool: "Edit".into(),
                files: vec!["src/auth.rs".into()],
                effect: ToolEffect::LocalMutation,
            }],
        };
        let md = plan.to_review_markdown("fix the auth bug");
        assert!(md.contains("## Task"));
        assert!(md.contains("fix the auth bug"));
        assert!(md.contains("## Goal"));
        assert!(md.contains("Fix auth"));
        assert!(md.contains("## Plan"));
        assert!(md.contains("Edit auth module"));
    }

    #[test]
    fn test_submit_review_parse_approved() {
        let args = json!({
            "verdict": "approved",
            "reasoning": "Plan looks good"
        });
        let result = SubmitReviewResult::from_tool_args(&args).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approved);
        assert_eq!(result.reasoning, "Plan looks good");
        assert!(result.suggested_changes.is_none());
    }

    #[test]
    fn test_submit_review_parse_rejected_with_changes() {
        let args = json!({
            "verdict": "rejected",
            "reasoning": "Missing error handling",
            "suggested_changes": ["Add error handling to step 2"]
        });
        let result = SubmitReviewResult::from_tool_args(&args).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Rejected);
        assert_eq!(result.suggested_changes.unwrap().len(), 1);
    }

    #[test]
    fn test_submit_review_invalid_verdict() {
        let args = json!({
            "verdict": "maybe",
            "reasoning": "unsure"
        });
        let err = SubmitReviewResult::from_tool_args(&args).unwrap_err();
        assert!(err.contains("invalid verdict"));
    }

    #[test]
    fn test_review_record_outcome_derived() {
        let rec = ReviewRecord {
            review_depth: ReviewDepth::SelfReview,
            reviewer_model: "test".into(),
            planner_model: "test".into(),
            plan_summary: "test".into(),
            reviewer_verdict: ReviewVerdict::Approved,
            reviewer_reasoning: None,
            human_decision: None,
            gate_reason: GateReason::ComplexityThreshold,
        };
        assert_eq!(rec.outcome(), HumanDecision::AcceptedPlan);

        let rec2 = ReviewRecord {
            human_decision: Some(HumanDecision::AcceptedReview),
            ..rec
        };
        assert_eq!(rec2.outcome(), HumanDecision::AcceptedReview);
    }

    #[test]
    fn test_gate_reason_display() {
        assert_eq!(
            GateReason::DestructiveFloor.to_string(),
            "destructive_floor"
        );
        assert_eq!(GateReason::RePlanExhausted.to_string(), "re_plan_exhausted");
    }
}
