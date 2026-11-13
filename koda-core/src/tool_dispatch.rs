//! Tool execution dispatch — sequential, parallel, and split-batch.
//!
//! Routes tool calls from the inference loop to execution, handling
//! approval flow, parallelization, and result recording.
//!
//! ## Dispatch flow
//!
//! ```text
//! Model emits tool calls
//!   → Classify each call's effect (ReadOnly / LocalMutation / Destructive)
//!   → Split into read-only batch + mutation batch
//!   → Read-only tools: execute in parallel (tokio::join)
//!   → Mutation tools: execute sequentially with approval
//!   → Record results in DB + inject into conversation
//! ```
//!
//! ## Related modules
//!
//! - [`crate::tools`] — tool definitions and `ToolRegistry::execute()`
//! - [`crate::trust`] — approval mode and effect classification
//! - `sub_agent_dispatch.rs` — `InvokeAgent` handling (needs provider access)
//! - `approval_flow.rs` — interactive approval UI flow
//!
//! ## Design (DESIGN.md)
//!
//! - **Tool Dispatch: Match Statement (P2)**: Tools are dispatched via a
//!   `match` in `ToolRegistry::execute()`, not a `HashMap<String, Box<dyn Tool>>`.
//!   Rust's exhaustive matching catches missing handlers at compile time.

use crate::approval_flow::{CommandInbox, handle_ask_user, request_approval};
use crate::db::Role;
use crate::engine::event::ToolOutcome;
use crate::engine::{ApprovalDecision, EngineEvent};
use crate::persistence::Persistence;
use crate::preview;
use crate::providers::ToolCall;
use crate::sub_agent_dispatch;
use crate::tools;
use crate::trust::{self, ToolApproval, TrustMode};
use crate::turn_context::{ToolExecutionContext, TurnContext};

use anyhow::Result;
use std::path::Path;
use tokio::sync::mpsc;

/// Post-execution recording: emit result event and persist to DB. Called after every successful tool execution regardless
/// of execution strategy (parallel, split-batch, or sequential).
///
/// Takes `&TurnContext` (not `ToolExecutionContext`) because spawner
/// identity is irrelevant to recording — only the per-turn ambient
/// fields are read.
pub(crate) async fn record_tool_result(
    tc: &ToolCall,
    result: &str,
    success: bool,
    full_output: Option<&str>,
    ctx: &TurnContext<'_>,
) -> Result<()> {
    let TurnContext {
        db,
        session_id,
        sink,
        tools,
        ..
    } = *ctx;
    let max_result_chars = tools.caps.tool_result_chars;
    sink.emit(EngineEvent::ToolCallResult {
        id: tc.id.clone(),
        name: tc.function_name.clone(),
        output: result.to_string(),
        outcome: success.into(),
    });

    // If we have separate full output (Bash smart summary), use the dedicated
    // two-column insert so the model sees the summary while RecallContext can
    // search the full output.
    if let Some(full) = full_output {
        db.insert_tool_message_with_full(session_id, result, &tc.id, full)
            .await?;
    } else {
        let stored = truncate_for_history(result, max_result_chars);
        db.insert_message(
            session_id,
            &Role::Tool,
            Some(&stored),
            None,
            Some(&tc.id),
            None,
        )
        .await?;
    }
    // (#1077 Phase B) `crate::progress::track_progress` was here. It
    // scraped tool outputs to maintain a parallel "engine sees what
    // the model just did" log that then re-injected into the system
    // prompt next turn. Removed alongside the system-prompt injection
    // it fed — the model owns its plan via `TodoWrite`, the
    // conversation history persists it, the engine surfaces
    // transitions via `EngineEvent::TodoUpdate`. See
    // `DESIGN.md § Progress Tracking`.
    Ok(())
}

/// Truncate a tool result for storage in conversation history.
/// The `max_chars` limit is set by `OutputCaps::tool_result_chars`.
fn truncate_for_history(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }
    // Find a safe char boundary
    let mut end = max_chars;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[...truncated {} chars. Re-read the file if you need the full content.]",
        &output[..end],
        output.len() - end
    )
}

/// Decide whether a batch of tool calls can run in parallel.
///
/// A batch is parallel-eligible iff every call in it (a) auto-approves
/// under the current trust mode and (b) doesn't conflict with another
/// call in the batch on the same target file.
pub(crate) fn can_parallelize(
    tool_calls: &[ToolCall],
    mode: TrustMode,
    project_root: &Path,
) -> bool {
    let all_approved = !tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            trust::check_tool(&tc.function_name, &args, mode, Some(project_root)),
            ToolApproval::NeedsConfirmation | ToolApproval::Blocked
        )
    });

    if !all_approved {
        return false;
    }

    !has_file_conflict(tool_calls)
}

/// Whether two mutating calls in the batch target the same file. Such a batch
/// has to run in the order the model gave it, so neither
/// [`execute_tools_parallel`] nor [`execute_tools_split_batch`] can take it:
/// concurrent Edits can each read the original file and lose the other's
/// change, and a split can put a later call first.
pub(crate) fn has_file_conflict(tool_calls: &[ToolCall]) -> bool {
    let mut seen = std::collections::HashSet::new();
    let catalog = crate::tools::ToolCatalog::default_static();
    tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        if !catalog.is_mutating_call(&tc.function_name, &args) {
            return false;
        }
        if let Some(path) = catalog
            .get_tool(&tc.function_name)
            .and_then(|tool| tool.mutated_path(&args))
        {
            // If the path is already in the set, we have a conflict
            !seen.insert(path.to_string_lossy().into_owned())
        } else {
            false
        }
    })
}

/// Execute a single tool call, returning (tool_call_id, result_output, success).
#[tracing::instrument(skip_all, fields(tool = %tc.function_name))]
pub(crate) async fn execute_one_tool(
    tc: &ToolCall,
    tx: ToolExecutionContext<'_>,
) -> (String, String, bool, Option<String>) {
    let TurnContext {
        session_id,
        tools,
        sink,
        sub_agent_cache,
        ..
    } = *tx.turn;
    let caller_spawner = tx.caller_spawner;
    let _session_id = session_id; // mirror the existing `_session_id` arg name
    let (result, success, full_output) = if tc.function_name == "InvokeAgent" {
        // Sub-agents inherit the parent's approval mode.
        //
        // Runtime invariant: the sub-agent dispatch loop short-circuits
        // `InvokeAgent` with a refusal (#1022 B7 revised), so this
        // branch is only ever reached from top-level inference. There
        // is no actual recursion at runtime.
        //
        // *Type*-level cycle still exists, however: `execute_one_tool`
        // calls `execute_sub_agent`, which calls `execute_one_tool` for
        // each of the sub-agent's *non-InvokeAgent* tool calls. The
        // borrow checker can't prove the runtime short-circuit, so it
        // sees a mutually-recursive `async fn` cycle and rejects the
        // future as infinitely sized (E0733). `Box::pin` breaks the
        // *type* cycle by erasing the future to `Pin<Box<dyn Future>>`.
        // The heap allocation is negligible — we already pay for
        // workspace setup, DB session, and a provider call.
        //
        // #1022 B10: bind the sender to `_` (drops immediately) rather
        // than `_dummy_tx` (lives until end of scope). With the sender
        // alive a sub-agent that hits `request_approval` would block
        // forever on `cmd_rx.recv()`. Dropping at construction makes
        // the channel closed from the receiver's perspective, which
        // `request_approval` already handles — it returns `None` and
        // the sub-agent dispatch loop maps that to a clean auto-reject
        // tool result the model can act on. Sub-agents have no path to
        // the user's prompt by design.
        let (_, mut dummy_rx) = mpsc::channel(1);
        let policy = tools.sandbox_policy().clone();
        let read_cache = tools.file_read_cache();
        let fut = sub_agent_dispatch::execute_sub_agent(
            tx,
            &tc.arguments,
            // Sub-agents get a fresh command channel (they auto-approve in all modes)
            &mut dummy_rx,
            Some(read_cache),
            // Phase 5 PR-4 of #934: hand the parent's effective policy
            // to the child so `compose()` can stack restrictions.
            &policy,
            // #1108 P2a: pass the InvokeAgent tool_call_id so the
            // narrative trace rows are folded under the parent's
            // `InvokeAgent` tool result in the persisted transcript.
            Some(&tc.id),
        );
        match Box::pin(fut).await {
            Ok(output) => (output, true, None),
            // **#1232 §4**: format with `{:#}` (anyhow's "alternate" Display)
            // so the entire context chain lands in the tool result string.
            // Pre-fix this used `{e}`, which only shows the topmost message
            // — a sub-agent dispatch failure caused by `reqwest::send()`
            // surfaced as `"Error invoking sub-agent: Failed to call LLM
            // API"` while the actually-useful `"connection refused" /
            // "timed out"` cause (added by `.context(...)` in the
            // provider) was silently dropped. The model has nothing to
            // act on. With `{e:#}` the chain renders as
            // `"Failed to call LLM API: error sending request: connection
            // refused"` and the model can self-correct (retry, switch
            // model, ask the user to check the network, ...).
            Err(e) => (format!("Error invoking sub-agent: {e:#}"), false, None),
        }
    } else if tc.function_name == "SpawnAgent" {
        // #1325 Phase 5a: SpawnAgent — codex v2 compatible peer-spawn.
        //
        // Same dispatch path as `InvokeAgent` but with a different
        // surface shape: `task_name` + `message` → `agent_name` + `prompt`.
        // Re-map the arguments before forwarding to `execute_sub_agent`
        // so the existing dispatch logic is reused without duplication.
        //
        // Box::pin is required for the same mutual-recursion reason as
        // `InvokeAgent` above — see that branch's comment.
        // Parse first — explicit error rather than silently becoming Null
        // and then getting a downstream "agent_name is required" error.
        let mut parsed: serde_json::Value = match serde_json::from_str(&tc.arguments) {
            Ok(v) => v,
            Err(e) => {
                return (
                    tc.id.clone(),
                    format!("SpawnAgent: argument JSON is malformed: {e}"),
                    false,
                    None,
                );
            }
        };
        if let Some(obj) = parsed.as_object_mut() {
            if let Some(task_name) = obj.remove("task_name") {
                obj.insert("agent_name".to_string(), task_name);
            }
            if let Some(message) = obj.remove("message") {
                obj.insert("prompt".to_string(), message);
            }
        }
        let remapped = serde_json::to_string(&parsed).unwrap_or_else(|_| tc.arguments.clone());
        let remapped_tc = crate::providers::ToolCall {
            function_name: "InvokeAgent".to_string(),
            arguments: remapped,
            id: tc.id.clone(),
            thought_signature: tc.thought_signature.clone(),
        };
        let (_, mut dummy_rx) = mpsc::channel(1);
        let policy = tools.sandbox_policy().clone();
        let read_cache = tools.file_read_cache();
        let fut = sub_agent_dispatch::execute_sub_agent(
            tx,
            &remapped_tc.arguments,
            &mut dummy_rx,
            Some(read_cache),
            &policy,
            Some(&remapped_tc.id),
        );
        match Box::pin(fut).await {
            Ok(output) => (output, true, None),
            Err(e) => (format!("Error spawning agent: {e:#}"), false, None),
        }
    } else {
        // Invalidate sub-agent cache on file mutations.
        //
        // Args-aware classification (#1265 PR-9): for `Bash` this
        // means `cat foo.txt` no longer wastes a cache invalidation,
        // while `rm foo.txt` still does.
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        if tools.catalog().is_mutating_call(&tc.function_name, &args) {
            sub_agent_cache.invalidate();
        }
        let streaming = if tc.function_name == "Bash" {
            Some((sink, tc.id.as_str()))
        } else {
            None
        };
        let r = tools
            .execute_for(
                &tc.function_name,
                &tc.arguments,
                streaming,
                caller_spawner,
                sink.can_ask(),
            )
            .await;
        (r.output, r.success, r.full_output)
    };

    (tc.id.clone(), result, success, full_output)
}

/// Pre-flight validate a tool call, then execute it.
///
/// Used by the parallel + split-batch arms (#1022 B14). The sequential
/// arm keeps its own pre-execute validation step because it runs *before*
/// approval prompting — we don't want to bother the user with a
/// confirmation that's guaranteed to fail. Parallel/split-batch only
/// reach this point when every tool was already classified `AutoApprove`,
/// so validate-then-execute is the right order.
async fn validate_then_execute_one_tool(
    tc: &ToolCall,
    tx: ToolExecutionContext<'_>,
) -> (String, String, bool, Option<String>) {
    let TurnContext {
        project_root,
        tools,
        ..
    } = *tx.turn;
    let parsed_args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();

    let validation_error = tools::validate::validate_with_registry(
        tools,
        &tc.function_name,
        &parsed_args,
        project_root,
    )
    .await;

    if let Some(error) = validation_error {
        return (
            tc.id.clone(),
            format!("Validation error: {error}"),
            false,
            None,
        );
    }

    execute_one_tool(tc, tx).await
}

/// Run multiple tool calls concurrently and store results.
pub(crate) async fn execute_tools_parallel(
    tool_calls: &[ToolCall],
    tx: ToolExecutionContext<'_>,
) -> Result<()> {
    let ctx = tx.turn;
    let count = tool_calls.len();
    ctx.sink.emit(EngineEvent::Info {
        message: format!("Running {count} tools in parallel..."),
    });

    // Launch all tool calls concurrently
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|tc| {
            // #1022 B14: validate before executing. The sequential arm
            // does this *before* approval; here every tool is already
            // AutoApproved (see `can_parallelize`) so validate-then-execute
            // is the right order.
            validate_then_execute_one_tool(tc, tx)
        })
        .collect();
    let names = tool_calls.iter().map(|tc| tc.function_name.as_str());
    let Some(results) = join_unless_cancelled(names, futures, &ctx.cancel).await else {
        return record_stopped_calls(tool_calls, ctx).await;
    };

    // Emit banner + result together so each tool's output is visually grouped
    for (i, (tc_id, result, success, full_output)) in results.into_iter().enumerate() {
        ctx.sink.emit(EngineEvent::ToolCallStart {
            id: tc_id.clone(),
            name: tool_calls[i].function_name.clone(),
            args: serde_json::from_str(&tool_calls[i].arguments).unwrap_or_default(),
            is_sub_agent: false,
        });
        record_tool_result(
            &tool_calls[i],
            &result,
            success,
            full_output.as_deref(),
            ctx,
        )
        .await?;
    }
    Ok(())
}

/// Split a mixed batch: run parallelizable tools concurrently, then
/// execute remaining tools sequentially.
///
/// This is the key optimization for mixed batches like
/// `[InvokeAgent, InvokeAgent, Write]` — the two sub-agents run in
/// parallel while the Write waits for confirmation.
pub(crate) async fn execute_tools_split_batch(
    tool_calls: &[ToolCall],
    tx: ToolExecutionContext<'_>,
    cmd_rx: &mut CommandInbox<'_>,
) -> Result<()> {
    let ctx = tx.turn;
    // Partition into parallelizable vs sequential
    let (parallel, sequential): (Vec<_>, Vec<_>) = tool_calls.iter().partition(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            trust::check_tool(&tc.function_name, &args, ctx.mode, Some(ctx.project_root),),
            ToolApproval::AutoApprove
        )
    });

    // Run parallelizable tools concurrently (if more than one)
    if parallel.len() > 1 {
        ctx.sink.emit(EngineEvent::Info {
            message: format!("Running {} tools in parallel...", parallel.len()),
        });

        let futures: Vec<_> = parallel
            .iter()
            .map(|tc| {
                // #1022 B14: validate before executing. Same reasoning
                // as `execute_tools_parallel` — every tool here is
                // already AutoApproved.
                validate_then_execute_one_tool(tc, tx)
            })
            .collect();
        let names = parallel.iter().map(|tc| tc.function_name.as_str());
        let Some(results) = join_unless_cancelled(names, futures, &ctx.cancel).await else {
            return record_stopped_calls(tool_calls, ctx).await;
        };

        for (j, (tc_id, result, success, full_output)) in results.into_iter().enumerate() {
            ctx.sink.emit(EngineEvent::ToolCallStart {
                id: tc_id.clone(),
                name: parallel[j].function_name.clone(),
                args: serde_json::from_str(&parallel[j].arguments).unwrap_or_default(),
                is_sub_agent: false,
            });
            record_tool_result(parallel[j], &result, success, full_output.as_deref(), ctx).await?;
        }
    } else {
        // 0–1 parallelizable tools — just run sequentially
        for tc in &parallel {
            let calls = std::slice::from_ref(*tc);
            execute_tools_sequential(calls, tx, cmd_rx).await?;
        }
    }

    // Run non-parallelizable tools sequentially
    if !sequential.is_empty() {
        let seq_calls: Vec<ToolCall> = sequential.into_iter().cloned().collect();
        execute_tools_sequential(&seq_calls, tx, cmd_rx).await?;
    }

    Ok(())
}

/// What the model reads for a call the user stopped before it ran.
const STOPPED_TOOL_RESULT: &str = "The user stopped the turn before this ran.";

/// Answer the calls a stopped turn left open.
///
/// An unanswered call gets its assistant message pruned on the next load, which
/// would hide what the model proposed and the results of the calls that ran.
async fn record_stopped_calls(tool_calls: &[ToolCall], ctx: &TurnContext<'_>) -> Result<()> {
    for tc in tool_calls {
        ctx.db
            .insert_message(
                ctx.session_id,
                &Role::Tool,
                Some(STOPPED_TOOL_RESULT),
                None,
                Some(&tc.id),
                None,
            )
            .await?;
    }
    Ok(())
}

/// End a call that started but never ran: the client hears how it ended,
/// and the model reads `text` as its result.
async fn record_unrun_call(
    tc: &ToolCall,
    text: &str,
    outcome: ToolOutcome,
    ctx: &TurnContext<'_>,
) -> Result<()> {
    ctx.sink.emit(EngineEvent::ToolCallResult {
        id: tc.id.clone(),
        name: tc.function_name.clone(),
        output: text.to_string(),
        outcome,
    });
    ctx.db
        .insert_message(
            ctx.session_id,
            &Role::Tool,
            Some(text),
            None,
            Some(&tc.id),
            None,
        )
        .await?;
    Ok(())
}

/// The turn stopped while `tool_calls[0]`, already started, waited or ran:
/// end it as cancelled and answer the rest.
async fn record_stopped_after_start(tool_calls: &[ToolCall], ctx: &TurnContext<'_>) -> Result<()> {
    if let Some((started, rest)) = tool_calls.split_first() {
        record_unrun_call(started, STOPPED_TOOL_RESULT, ToolOutcome::Cancelled, ctx).await?;
        record_stopped_calls(rest, ctx).await?;
    }
    Ok(())
}

/// Run tool calls one at a time (when confirmation is needed, or single call).
pub(crate) async fn execute_tools_sequential(
    tool_calls: &[ToolCall],
    tx: ToolExecutionContext<'_>,
    cmd_rx: &mut CommandInbox<'_>,
) -> Result<()> {
    let ctx = tx.turn;
    let TurnContext {
        project_root,
        tools,
        mode,
        sink,
        ref cancel,
        session_allows,
        ..
    } = *ctx;
    for (i, tc) in tool_calls.iter().enumerate() {
        // Check for interrupt before each tool
        if cancel.is_cancelled() {
            sink.emit(EngineEvent::Warn {
                message: "Interrupted".into(),
            });
            return record_stopped_calls(&tool_calls[i..], ctx).await;
        }

        let parsed_args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or_default();

        sink.emit(EngineEvent::ToolCallStart {
            id: tc.id.clone(),
            name: tc.function_name.clone(),
            args: parsed_args.clone(),
            is_sub_agent: false,
        });

        // AskUser: pause inference, show question in TUI, wait for typed answer.
        // Handled here (not in execute_one_tool) because it needs sink + cmd_rx.
        if tc.function_name == "AskUser" {
            let answer = handle_ask_user(sink, cmd_rx, cancel, &tc.id, &parsed_args).await;
            let result = match answer {
                Some(text) if !text.trim().is_empty() => text,
                Some(_) => "User did not provide an answer.".into(),
                None => return record_stopped_after_start(&tool_calls[i..], ctx).await,
            };
            record_tool_result(tc, &result, true, None, ctx).await?;
            continue;
        }

        // Pre-flight validation: catch errors before bothering the user
        // with an approval prompt that will inevitably fail.
        if let Some(error) = tools::validate::validate_with_registry(
            tools,
            &tc.function_name,
            &parsed_args,
            project_root,
        )
        .await
        {
            record_tool_result(tc, &format!("Validation error: {error}"), false, None, ctx).await?;
            continue;
        }

        let mut approval = trust::check_tool_with_registry(
            &tc.function_name,
            &parsed_args,
            mode,
            Some(project_root),
            tools,
        );
        // Stopping a background process Koda started is a local mutation,
        // not the destructive `kill` it reads as: Auto doesn't ask.
        if mode == TrustMode::Auto
            && approval == ToolApproval::NeedsConfirmation
            && tc.function_name == "Bash"
            && tools::shell::signals_only_own_background(
                &parsed_args,
                &tools.bg_registry,
                tx.caller_spawner,
            )
        {
            approval = ToolApproval::AutoApprove;
        }
        // A `kill` of Koda's own processes inside a longer command can't work
        // in the sandbox, approved or not: say how instead of asking.
        if mode != TrustMode::Plan
            && tc.function_name == "Bash"
            && let Some(hint) = tools::shell::own_kill_in_longer_command(
                &parsed_args,
                &tools.bg_registry,
                tx.caller_spawner,
            )
        {
            record_tool_result(tc, &hint, false, None, ctx).await?;
            continue;
        }

        // A yes "for this session" covers the same command or file again.
        let effect = (approval == ToolApproval::NeedsConfirmation).then(|| {
            crate::trust::resolve_tool_effect_with_registry(&tc.function_name, &parsed_args, tools)
        });
        let session_key = effect.and_then(|effect| {
            trust::session_allow_key(&tc.function_name, &parsed_args, effect, project_root)
        });
        if session_key
            .as_deref()
            .is_some_and(|key| session_allows.contains(key))
        {
            approval = ToolApproval::AutoApprove;
        }

        match approval {
            ToolApproval::AutoApprove => {
                // Execute without asking
            }
            ToolApproval::Blocked => {
                // Plan mode: emit ActionBlocked event, let the client render it
                let detail = tools::describe_action(&tc.function_name, &parsed_args);
                let diff_preview =
                    preview::compute(&tc.function_name, &parsed_args, project_root).await;
                sink.emit(EngineEvent::ActionBlocked {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.function_name.clone(),
                    detail: detail.clone(),
                    preview: diff_preview,
                });
                record_unrun_call(
                    tc,
                    crate::trust::PLAN_BLOCKED_MESSAGE,
                    ToolOutcome::Declined,
                    ctx,
                )
                .await?;
                continue;
            }
            ToolApproval::NeedsConfirmation => {
                let detail = tools::describe_action(&tc.function_name, &parsed_args);
                let diff_preview =
                    preview::compute(&tc.function_name, &parsed_args, project_root).await;
                let effect = effect.expect("computed for every NeedsConfirmation");

                match request_approval(
                    sink,
                    cmd_rx,
                    cancel,
                    &tc.id,
                    &tc.function_name,
                    &detail,
                    diff_preview,
                    effect,
                    session_key.is_some(),
                )
                .await
                {
                    Some(ApprovalDecision::Approve) => {}
                    Some(ApprovalDecision::ApproveForSession) => {
                        if let Some(key) = session_key {
                            session_allows.insert(key);
                        }
                    }
                    Some(ApprovalDecision::Reject) => {
                        record_unrun_call(
                            tc,
                            "User rejected this action.",
                            ToolOutcome::Declined,
                            ctx,
                        )
                        .await?;
                        continue;
                    }
                    Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                        let result = format!("User rejected this action with feedback: {feedback}");
                        record_unrun_call(tc, &result, ToolOutcome::Declined, ctx).await?;
                        continue;
                    }
                    Some(ApprovalDecision::RejectAuto { reason }) => {
                        // #1022 B15: distinct from Reject so the model knows
                        // there's no human in the loop — it should adapt its
                        // plan to the structural constraint, not ask for
                        // clarification.
                        let result = format!("[auto-rejected: {reason}]");
                        record_unrun_call(tc, &result, ToolOutcome::Declined, ctx).await?;
                        continue;
                    }
                    None => return record_stopped_after_start(&tool_calls[i..], ctx).await,
                }
            }
        }

        let (_, result, success, full_output) = if stops_on_cancel(&tc.function_name) {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return record_stopped_after_start(&tool_calls[i..], ctx).await;
                }
                outcome = execute_one_tool(tc, tx) => outcome,
            }
        } else {
            execute_one_tool(tc, tx).await
        };
        record_tool_result(tc, &result, success, full_output.as_deref(), ctx).await?;
    }
    Ok(())
}

/// Wait for a parallel batch. When every call in it is one a cancel may
/// drop ([`stops_on_cancel`]), a cancel drops the batch and returns `None`;
/// a batch with an edit or a sub-agent in it runs to the end.
async fn join_unless_cancelled<'n, F: std::future::Future>(
    mut names: impl Iterator<Item = &'n str>,
    futures: Vec<F>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<Vec<F::Output>> {
    let all = futures_util::future::join_all(futures);
    if !names.all(stops_on_cancel) {
        return Some(all.await);
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        results = all => Some(results),
    }
}

/// Tools a cancel may drop mid-call: they only read, or run a command whose
/// whole process group dies with the call (Bash and Cargo hold a
/// `ProcessGroup`). Without this a running command held the turn until the
/// engine gave up on it after `CANCEL_GRACE` and reported the turn as failed.
/// File edits and sub-agents are left to finish: an edit is quick, and
/// dropping one halfway could leave a file half written.
fn stops_on_cancel(tool: &str) -> bool {
    matches!(
        tool,
        "Bash" | "Cargo" | "WebFetch" | "WebSearch" | "Grep" | "Glob" | "List" | "Read"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ToolCall;

    fn make_tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".to_string(),
            function_name: name.to_string(),
            arguments: "{}".to_string(),
            thought_signature: None,
        }
    }

    #[test]
    fn test_can_parallelize_read_only() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Grep")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_cannot_parallelize_writes() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_cannot_parallelize_bash() {
        // Dangerous bash command should prevent parallelization
        let calls = vec![
            make_tool_call("Read"),
            ToolCall {
                id: "t2".to_string(),
                function_name: "Bash".to_string(),
                arguments: r#"{"command": "rm -rf /tmp/test"}"#.to_string(),
                thought_signature: None,
            },
        ];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_can_parallelize_agents() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("InvokeAgent")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_cannot_parallelize_same_file_edits() {
        let calls = vec![
            ToolCall {
                id: "t1".to_string(),
                function_name: "Edit".to_string(),
                arguments: r#"{"file_path": "src/main.rs"}"#.to_string(),
                thought_signature: None,
            },
            ToolCall {
                id: "t2".to_string(),
                function_name: "Edit".to_string(),
                arguments: r#"{"file_path": "src/main.rs"}"#.to_string(),
                thought_signature: None,
            },
        ];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Auto, // Auto mode would normally allow parallelization
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_can_parallelize_different_file_edits() {
        let calls = vec![
            ToolCall {
                id: "t1".to_string(),
                function_name: "Edit".to_string(),
                arguments: r#"{"file_path": "src/main.rs"}"#.to_string(),
                thought_signature: None,
            },
            ToolCall {
                id: "t2".to_string(),
                function_name: "Edit".to_string(),
                arguments: r#"{"file_path": "src/lib.rs"}"#.to_string(),
                thought_signature: None,
            },
        ];
        assert!(can_parallelize(
            &calls,
            TrustMode::Auto,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_is_mutating_tool() {
        // Post-#1265 PR-9: classification flows through the catalog
        // (per-call, args-aware). For args-insensitive tools the
        // result is identical to the legacy name-only check; for
        // `Bash` it depends on the command — here we pass `Null`
        // which `BashTool::classify` treats as "unknown command,
        // assume worst case" → mutating.
        let cat = crate::tools::ToolCatalog::default_static();
        let null = serde_json::Value::Null;
        assert!(cat.is_mutating_call("Write", &null));
        assert!(cat.is_mutating_call("Edit", &null));
        assert!(cat.is_mutating_call("Bash", &null));
        assert!(cat.is_mutating_call("MemoryWrite", &null));
        assert!(!cat.is_mutating_call("Read", &null));
        assert!(!cat.is_mutating_call("List", &null));
        // InvokeAgent is ReadOnly (sub-agents inherit parent's approval mode)
        assert!(!cat.is_mutating_call("InvokeAgent", &null));

        // Args-aware showcase: Bash with a benign command is
        // ReadOnly; this is the bug the legacy name-only function
        // could not catch.
        let echo = serde_json::json!({"command": "echo hello"});
        assert!(!cat.is_mutating_call("Bash", &echo));
    }

    #[test]
    fn test_mixed_batch_not_fully_parallelizable() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
        ));
    }

    #[test]
    fn test_mixed_batch_fully_parallelizable_in_auto() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Auto,
            Path::new("/test/project"),
        ));
    }
}
