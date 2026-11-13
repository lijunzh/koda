//! Turns: one request from the user and everything Koda does for it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A turn. `turn/started` sends it in progress, `turn/completed` finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    /// The turn's id.
    pub id: String,
    /// Where it stands.
    pub status: TurnStatus,
    /// Why it failed, when it did.
    pub error: Option<TurnError>,
    /// What it used: tokens and model calls, sub-agents and compaction
    /// included. Set when the turn is over.
    pub usage: Option<Usage>,
    /// Wall-clock time, set when the turn is over.
    pub duration_ms: Option<u64>,
}

/// Where a turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// Running.
    InProgress,
    /// The model finished.
    Completed,
    /// The user or client stopped it.
    Interrupted,
    /// It failed, after any retries.
    Failed,
}

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    /// One sentence for a person.
    pub message: String,
}

/// Tokens and model calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens read from the provider's cache.
    pub cached_input_tokens: u64,
    /// Input tokens written to the provider's cache.
    pub cache_creation_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Output tokens spent reasoning.
    pub reasoning_tokens: u64,
    /// Model calls that finished.
    pub model_calls: u64,
}

/// `turn/started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    /// The session.
    pub session_id: String,
    /// The turn, in progress.
    pub turn: Turn,
}

/// `turn/completed`: the turn is over, however it ended. Exactly one per
/// turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    /// The session.
    pub session_id: String,
    /// The turn, finished.
    pub turn: Turn,
}

/// `turn/plan/updated`: the model rewrote its plan (its todo list).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnPlanUpdatedNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The whole plan, in order.
    pub plan: Vec<PlanStep>,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    /// What the step is.
    pub step: String,
    /// How far it got.
    pub status: PlanStepStatus,
}

/// How far a plan step got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlanStepStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
}

/// `turn/loopGuard`: the model kept repeating itself. Koda's own; Codex has
/// none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TurnLoopGuardNotification {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// What Koda did about it.
    pub action: LoopGuardAction,
    /// What was repeated.
    pub detail: String,
}

/// What the loop guard did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LoopGuardAction {
    /// Told the model to change course; the turn goes on.
    Feedback,
    /// The model didn't, so the turn stops.
    Stop,
}

/// `turn/loopCap/requestDecision`: the turn reached its iteration cap.
/// Until it is answered the turn waits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopCapParams {
    /// The session.
    pub session_id: String,
    /// The turn.
    pub turn_id: String,
    /// The cap.
    pub cap: u32,
    /// The tools called most recently.
    pub recent_tools: Vec<String>,
}

/// The answer to `turn/loopCap/requestDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoopCapResponse {
    /// Go on or stop.
    pub decision: LoopCapDecision,
}

/// Whether a turn at its cap goes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LoopCapDecision {
    /// 50 more iterations.
    Continue50,
    /// 200 more iterations.
    Continue200,
    /// Stop the turn.
    Stop,
}
