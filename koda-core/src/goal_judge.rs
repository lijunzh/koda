//! The goal evaluator: with `goal_judge = "evaluator"`, a check after each
//! turn on an active goal decides whether it is achieved, as Claude Code's
//! `/goal` does with a prompt-based Stop hook.
//!
//! The check is one more model call after the turn's last reply. It sends
//! the turn's own request (the same system prompt, history and tools, on
//! the same model) with the question after it, and says no tool may be
//! called. A server that caches prompts by prefix (llama.cpp, LM Studio,
//! the Claude API) then reads only the question and the answer as new
//! tokens; a request without the tools would miss that cache on the many
//! chat templates that render tools near the start. Claude Code asks its
//! small fast model instead; Koda's local models have no second model to
//! spare.
//!
//! - **Met:** the goal is marked complete.
//! - **Impossible:** it is marked blocked, so `/goal resume` can try again
//!   (Claude Code clears it).
//! - **Not yet:** the reason goes to the host, which starts the next turn
//!   with it as guidance.
//!
//! An answer that can't be read counts as not yet, without a reason. The
//! call's tokens count toward the turn and the goal.

use std::time::Instant;

use koda_state::db::Database;
use koda_state::db::goals::{GoalStatus, GoalUpdate};
use tokio_util::sync::CancellationToken;

use crate::engine::{EngineEvent, EngineSink};
use crate::goal::{Goal, GoalVerdict};
use crate::providers::{ChatMessage, LlmProvider, StreamChunk, ToolDefinition};

/// The most the evaluator may write, thinking included: room for a model
/// that thinks before it answers, and a bound on one that doesn't stop.
const MAX_ANSWER_TOKENS: u32 = 8_192;

/// The turn's request, for the evaluator to ask after.
pub(crate) struct TurnRequest<'a> {
    pub provider: &'a dyn LlmProvider,
    /// The turn's history, its last reply included.
    pub messages: Vec<ChatMessage>,
    pub tool_defs: &'a [ToolDefinition],
    pub settings: &'a crate::config::ModelSettings,
}

/// Judge the session's active goal after a turn, and act on the verdict.
/// Nothing happens when the goal isn't active, or the call is cancelled or
/// fails (logged).
pub(crate) async fn judge_turn(
    db: &Database,
    session_id: &str,
    request: TurnRequest<'_>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
) {
    let goal = match db.get_goal(session_id).await {
        Ok(Some(goal)) if goal.status == GoalStatus::Active => goal,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("couldn't read the goal to judge it: {e:#}");
            return;
        }
    };
    sink.emit(EngineEvent::SpinnerStart {
        message: "Checking the goal...".into(),
    });
    let answer = ask(&goal, request, sink, cancel).await;
    sink.emit(EngineEvent::SpinnerStop);
    let reply = match answer {
        Ok(Some(reply)) => reply,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(goal_id = %goal.goal_id, "the goal check failed: {e:#}");
            return;
        }
    };
    let (verdict, reason) = crate::goal::parse_verdict(&reply).unwrap_or_else(|| {
        tracing::warn!(
            goal_id = %goal.goal_id,
            reply_chars = reply.len(),
            "the goal check gave no verdict; counted as not yet"
        );
        (GoalVerdict::NotYet, None)
    });
    tracing::info!(goal_id = %goal.goal_id, ?verdict, reason = ?reason, "goal checked");
    sink.emit(EngineEvent::GoalJudged {
        verdict,
        reason: reason.clone(),
    });
    let status = match verdict {
        GoalVerdict::Met => GoalStatus::Complete,
        GoalVerdict::Impossible => GoalStatus::Blocked,
        GoalVerdict::NotYet => return,
    };
    let update = GoalUpdate {
        status: Some(status),
        expected_goal_id: Some(goal.goal_id.clone()),
        ..Default::default()
    };
    match db.update_goal(session_id, update).await {
        Ok(Some(goal)) => sink.emit(EngineEvent::GoalUpdated { goal }),
        Ok(None) => {}
        Err(e) => tracing::warn!("couldn't record the goal check: {e:#}"),
    }
}

/// Ask, and read the whole answer. `None` when cancelled.
async fn ask(
    goal: &Goal,
    request: TurnRequest<'_>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<String>> {
    let TurnRequest {
        provider,
        mut messages,
        tool_defs,
        settings,
    } = request;
    messages.push(ChatMessage::text(
        "user",
        &crate::goal::evaluator_message(goal),
    ));
    let settings = crate::config::ModelSettings {
        no_tool_calls: true,
        max_tokens: Some(
            settings
                .max_tokens
                .map_or(MAX_ANSWER_TOKENS, |cap| cap.min(MAX_ANSWER_TOKENS)),
        ),
        ..settings.clone()
    };
    let started = Instant::now();
    let collector = tokio::select! {
        result = provider.chat_stream(&messages, tool_defs, &settings) => result?,
        _ = cancel.cancelled() => return Ok(None),
    };
    let mut rx = collector.rx;
    let mut text = String::new();
    let mut first_chunk = None;
    let mut usage = None;
    loop {
        let chunk = tokio::select! {
            chunk = rx.recv() => chunk,
            _ = cancel.cancelled() => {
                collector.handle.abort();
                return Ok(None);
            }
        };
        first_chunk.get_or_insert_with(|| started.elapsed());
        match chunk {
            Some(StreamChunk::TextDelta(delta)) => text.push_str(&delta),
            Some(StreamChunk::Done(done)) => usage = Some(done),
            Some(StreamChunk::NetworkError(error)) => anyhow::bail!("{error}"),
            Some(_) => {}
            None => break,
        }
    }
    let usage = usage.unwrap_or_default();
    sink.emit(EngineEvent::ModelUsage {
        usage: crate::engine::UsageTotals::of_call(provider, &usage),
    });
    crate::inference_helpers::ModelCall {
        purpose: "goal_check",
        system_hash: None,
        tools_hash: None,
        provider: provider.provider_name(),
        model: &settings.model,
        effort: crate::model_effort::sent(
            provider.provider_name(),
            &settings.model,
            settings.reasoning_effort.as_deref(),
        ),
        messages: messages.len(),
        estimated_tokens: crate::inference_helpers::estimate_tokens(&messages),
        context_tokens: provider.context_tokens(&usage),
        first_chunk,
        elapsed: started.elapsed(),
        usage: &usage,
        tool_calls: 0,
        text_chars: text.len(),
    }
    .log();
    Ok(Some(text))
}
