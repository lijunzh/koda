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

use crate::approval_flow::{handle_ask_user, request_approval};
use crate::db::Role;
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::file_tracker::FileTracker;
use crate::persistence::Persistence;
use crate::preview;
use crate::providers::ToolCall;
use crate::sub_agent_dispatch;
use crate::tools;
use crate::trust::{self, ToolApproval, TrustMode};
use crate::turn_context::{ToolExecutionContext, TurnContext};

use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Post-execution recording: emit result event, persist to DB, track progress
/// and file lifecycle. Called after every successful tool execution regardless
/// of execution strategy (parallel, split-batch, or sequential).
///
/// Takes `&TurnContext` (not `ToolExecutionContext`) because spawner
/// identity is irrelevant to recording — only the per-turn ambient
/// fields are read. `file_tracker` stays as a separate `&mut` arg per
/// the design rationale in `crate::turn_context` module docs.
pub(crate) async fn record_tool_result(
    tc: &ToolCall,
    result: &str,
    success: bool,
    full_output: Option<&str>,
    ctx: &TurnContext<'_>,
    file_tracker: &mut FileTracker,
) -> Result<()> {
    let TurnContext {
        db,
        session_id,
        project_root,
        sink,
        tools,
        ..
    } = *ctx;
    let max_result_chars = tools.caps.tool_result_chars;
    sink.emit(EngineEvent::ToolCallResult {
        id: tc.id.clone(),
        name: tc.function_name.clone(),
        output: result.to_string(),
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
    let parsed_args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
    track_file_lifecycle(
        &tc.function_name,
        &parsed_args,
        project_root,
        file_tracker,
        success,
    )
    .await;
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

/// Resolve the file path from a tool call's arguments.
///
/// Used by the file lifecycle tracker to record which paths
/// Koda creates or deletes (#465). Only relevant for Write and Delete.
fn resolve_tool_path(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
) -> Option<PathBuf> {
    if !matches!(tool_name, "Write" | "Delete") {
        return None;
    }
    crate::file_tracker::resolve_file_path_from_args(args, project_root)
}

/// Update file lifecycle tracker after a tool execution (#465).
///
/// - Write → track as owned (Koda created it)
/// - Delete → untrack (file no longer exists)
///
/// Only tracks when `success` is true, using the structured boolean
/// from `ToolResult` rather than fragile string-prefix matching (#476).
async fn track_file_lifecycle(
    tool_name: &str,
    args: &serde_json::Value,
    project_root: &Path,
    file_tracker: &mut FileTracker,
    success: bool,
) {
    if !success {
        return;
    }
    if let Some(path) = resolve_tool_path(tool_name, args, project_root) {
        match tool_name {
            "Write" => file_tracker.track_created(path).await,
            "Delete" => file_tracker.untrack(&path).await,
            _ => {}
        }
    }
}

/// Decide whether a batch of tool calls can run in parallel.
///
/// A batch is parallel-eligible iff every call in it (a) auto-approves
/// under the current trust mode and (b) doesn't conflict with another
/// call in the batch on the same target file.
///
/// **#1022 B13**: this used to call [`trust::check_tool`] (no
/// `FileTracker`), which is *not* the same classification the
/// sequential dispatch loop uses. Sequential calls
/// [`trust::check_tool_with_tracker`] so that `Delete` of a
/// Koda-owned file (created via `Write` earlier this session)
/// downgrades from `NeedsConfirmation` to `AutoApprove` per #465. The
/// mismatch meant batches like `[Read other.txt, Delete owned.tmp]`
/// were spuriously refused parallelization — each tool was eligible
/// in isolation, but the batch fell into the slower split-batch /
/// sequential path. Pure perf regression, no correctness impact, but
/// the kind of invariant violation that grows teeth over time as
/// other path-aware downgrades get added to the tracker path.
///
/// Now takes the same `Option<&FileTracker>` the sequential loop
/// passes, and forwards it to `check_tool_with_tracker`. Same
/// classification, same answer. Tests guard the regression below
/// (`test_can_parallelize_delete_owned_file_uses_tracker`).
pub(crate) fn can_parallelize(
    tool_calls: &[ToolCall],
    mode: TrustMode,
    project_root: &Path,
    file_tracker: Option<&crate::file_tracker::FileTracker>,
) -> bool {
    let all_approved = !tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            trust::check_tool_with_tracker(
                &tc.function_name,
                &args,
                mode,
                Some(project_root),
                file_tracker,
            ),
            ToolApproval::NeedsConfirmation | ToolApproval::Blocked
        )
    });

    if !all_approved {
        return false;
    }

    let mut seen = std::collections::HashSet::new();
    let catalog = crate::tools::ToolCatalog::default_static();
    let has_conflict = tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        if !catalog.is_mutating_call(&tc.function_name, &args) {
            return false;
        }
        // Per-tool undo path now lives on the trait — single source
        // of truth for which tools snapshot which arg.
        if let Some(path) = catalog
            .get_tool(&tc.function_name)
            .and_then(|tool| tool.extract_undo_path(&args))
        {
            // If the path is already in the set, we have a conflict
            !seen.insert(path.to_string_lossy().into_owned())
        } else {
            false
        }
    });

    !has_conflict
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
        bg_agents,
        ref cancel,
        ..
    } = *tx.turn;
    let caller_spawner = tx.caller_spawner;
    let _session_id = session_id; // mirror the existing `_session_id` arg name
    let (result, success, full_output) = if matches!(
        tc.function_name.as_str(),
        "ListBackgroundTasks" | "CancelTask" | "WaitTask"
    ) {
        // Layer 2 of #996 — background-task management tools.
        //
        // These need the `Arc<ChildAgentRegistry>` (not held by the
        // ToolRegistry) plus the caller's spawner identity (now
        // threaded as `caller_spawner`), so they can't go through
        // the generic `tools.execute()` path.
        let r = crate::tools::bg_task_tools::execute(
            &tc.function_name,
            &tc.arguments,
            bg_agents,
            &tools.bg_registry,
            caller_spawner,
            cancel,
        )
        .await;
        (r.output, r.success, r.full_output)
    } else if tc.function_name == "InvokeAgent" {
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
            // Layer 4 of #996 + #1076: foreground sub-agents are not
            // tracked in the bg-agent registry, so there is no
            // `ChildStatusEmitter` to fan out per-iteration heartbeats
            // to. Pass `None` to skip the per-iteration emit.
            None,
            // #1108 P2a: pass the InvokeAgent tool_call_id so any
            // bg-sub-agent reservation can record it. The drain
            // hook in the inference loop will persist the bg sub-
            // agent's narrative trace to `session_events` keyed by
            // this id, so the transcript renderer can fold it under
            // the parent's `InvokeAgent` tool result.
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
            .execute(&tc.function_name, &tc.arguments, streaming, caller_spawner)
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
    file_tracker: &mut FileTracker,
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
    let results = futures_util::future::join_all(futures).await;

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
            file_tracker,
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
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    file_tracker: &mut FileTracker,
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
        let results = futures_util::future::join_all(futures).await;

        for (j, (tc_id, result, success, full_output)) in results.into_iter().enumerate() {
            ctx.sink.emit(EngineEvent::ToolCallStart {
                id: tc_id.clone(),
                name: parallel[j].function_name.clone(),
                args: serde_json::from_str(&parallel[j].arguments).unwrap_or_default(),
                is_sub_agent: false,
            });
            record_tool_result(
                parallel[j],
                &result,
                success,
                full_output.as_deref(),
                ctx,
                file_tracker,
            )
            .await?;
        }
    } else {
        // 0–1 parallelizable tools — just run sequentially
        for tc in &parallel {
            let calls = std::slice::from_ref(*tc);
            execute_tools_sequential(calls, tx, cmd_rx, file_tracker).await?;
        }
    }

    // Run non-parallelizable tools sequentially
    if !sequential.is_empty() {
        let seq_calls: Vec<ToolCall> = sequential.into_iter().cloned().collect();
        execute_tools_sequential(&seq_calls, tx, cmd_rx, file_tracker).await?;
    }

    Ok(())
}

/// Run tool calls one at a time (when confirmation is needed, or single call).
pub(crate) async fn execute_tools_sequential(
    tool_calls: &[ToolCall],
    tx: ToolExecutionContext<'_>,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    file_tracker: &mut FileTracker,
) -> Result<()> {
    let ctx = tx.turn;
    let TurnContext {
        project_root,
        db,
        session_id,
        tools,
        mode,
        sink,
        ref cancel,
        ..
    } = *ctx;
    for tc in tool_calls {
        // Check for interrupt before each tool
        if cancel.is_cancelled() {
            sink.emit(EngineEvent::Warn {
                message: "Interrupted".into(),
            });
            return Ok(());
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
            let answer = handle_ask_user(sink, cmd_rx, cancel, &parsed_args).await;
            let result = match answer {
                Some(text) if !text.trim().is_empty() => text,
                Some(_) => "User did not provide an answer.".into(),
                None => return Ok(()), // cancelled
            };
            record_tool_result(tc, &result, true, None, ctx, file_tracker).await?;
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
            record_tool_result(
                tc,
                &format!("Validation error: {error}"),
                false,
                None,
                ctx,
                file_tracker,
            )
            .await?;
            continue;
        }

        // Check approval for this tool call (with file ownership awareness, #465)
        let approval = trust::check_tool_with_tracker(
            &tc.function_name,
            &parsed_args,
            mode,
            Some(project_root),
            Some(file_tracker),
        );

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
                    tool_name: tc.function_name.clone(),
                    detail: detail.clone(),
                    preview: diff_preview,
                });
                db.insert_message(
                    session_id,
                    &Role::Tool,
                    Some("[safe mode] Action blocked. You are in read-only mode. DO NOT retry this command. Describe what you would do instead. The user must press Shift+Tab to switch to auto or strict mode."),
                    None,
                    Some(&tc.id),
                    None,
                )
                .await?;
                continue;
            }
            ToolApproval::NeedsConfirmation => {
                let detail = tools::describe_action(&tc.function_name, &parsed_args);
                let diff_preview =
                    preview::compute(&tc.function_name, &parsed_args, project_root).await;
                let effect = crate::trust::resolve_tool_effect_with_registry(
                    &tc.function_name,
                    &parsed_args,
                    tools,
                );

                match request_approval(
                    sink,
                    cmd_rx,
                    cancel,
                    &tc.function_name,
                    &detail,
                    diff_preview,
                    effect,
                )
                .await
                {
                    Some(ApprovalDecision::Approve) => {}
                    Some(ApprovalDecision::Reject) => {
                        db.insert_message(
                            session_id,
                            &Role::Tool,
                            Some("User rejected this action."),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }
                    Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                        let result = format!("User rejected this action with feedback: {feedback}");
                        db.insert_message(
                            session_id,
                            &Role::Tool,
                            Some(&result),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }
                    Some(ApprovalDecision::RejectAuto { reason }) => {
                        // #1022 B15: distinct from Reject so the model knows
                        // there's no human in the loop — it should adapt its
                        // plan to the structural constraint, not ask for
                        // clarification.
                        let result = format!("[auto-rejected: {reason}]");
                        db.insert_message(
                            session_id,
                            &Role::Tool,
                            Some(&result),
                            None,
                            Some(&tc.id),
                            None,
                        )
                        .await?;
                        continue;
                    }
                    None => {
                        // Cancelled
                        return Ok(());
                    }
                }
            }
        }

        let (_, result, success, full_output) = execute_one_tool(tc, tx).await;
        record_tool_result(
            tc,
            &result,
            success,
            full_output.as_deref(),
            ctx,
            file_tracker,
        )
        .await?;
    }
    Ok(())
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
            None,
        ));
    }

    #[test]
    fn test_cannot_parallelize_writes() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
            None,
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
            None,
        ));
    }

    #[test]
    fn test_can_parallelize_agents() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("InvokeAgent")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project"),
            None,
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
            None,
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
            None,
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
        assert!(cat.is_mutating_call("Delete", &null));
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
            None,
        ));
    }

    #[test]
    fn test_mixed_batch_fully_parallelizable_in_auto() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Auto,
            Path::new("/test/project"),
            None,
        ));
    }

    /// #1022 B13 regression: `can_parallelize` must use the same
    /// approval classification the sequential dispatch loop uses,
    /// i.e. `check_tool_with_tracker` not `check_tool`. Without the
    /// tracker, `Delete owned.tmp` looks like `NeedsConfirmation`
    /// (because Delete is Destructive in Safe mode); with the tracker
    /// it auto-approves (#465: Koda created it, Koda removes it). The
    /// bug spuriously refused parallelization for batches that
    /// included a Delete of a file Koda created earlier in the
    /// session — pure perf regression, but the kind of
    /// classification mismatch that compounds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_can_parallelize_delete_owned_file_uses_tracker() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = crate::db::Database::open(&dir.path().join("test.db"))
            .await
            .unwrap();
        let mut tracker = crate::file_tracker::FileTracker::new("test-sess", db).await;
        // Canonicalize root so the tracked path matches what
        // `resolve_file_path_from_args` produces at lookup time — on
        // macOS, tempdirs live under `/var/folders/...` but
        // `canonicalize()` resolves to `/private/var/folders/...`.
        // Production code goes through canonicalization on both write
        // and lookup, so we mirror that here.
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let owned_abs = root.join("temp_output.md");
        std::fs::write(&owned_abs, "").unwrap();
        tracker
            .track_created(owned_abs.canonicalize().unwrap())
            .await;

        // Batch: Read other.txt + Delete owned.tmp. Both auto-approve
        // when the tracker is consulted; without the tracker the
        // Delete is misclassified as NeedsConfirmation.
        let calls = vec![
            ToolCall {
                id: "t1".to_string(),
                function_name: "Read".to_string(),
                arguments: r#"{"path": "other.txt"}"#.to_string(),
                thought_signature: None,
            },
            ToolCall {
                id: "t2".to_string(),
                function_name: "Delete".to_string(),
                arguments: r#"{"path": "temp_output.md"}"#.to_string(),
                thought_signature: None,
            },
        ];

        // Bug repro: without the tracker, Safe mode refuses
        // parallelization because Delete → NeedsConfirmation.
        assert!(
            !can_parallelize(&calls, TrustMode::Safe, &root, None),
            "sanity: without tracker, Delete must look like NeedsConfirmation"
        );

        // Fix proof: with the tracker, Delete of owned file
        // auto-approves → batch is parallelizable.
        assert!(
            can_parallelize(&calls, TrustMode::Safe, &root, Some(&tracker)),
            "with tracker, Delete of Koda-owned file must be \
             parallel-eligible (matches sequential path classification)"
        );
    }
}
