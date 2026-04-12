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
//! - [`crate::approval`] — approval mode and effect classification
//! - `sub_agent_dispatch.rs` — `InvokeAgent` handling (needs provider access)
//! - `approval_flow.rs` — interactive approval UI flow
//!
//! ## Design (DESIGN.md)
//!
//! - **Tool Dispatch: Match Statement (P2)**: Tools are dispatched via a
//!   `match` in `ToolRegistry::execute()`, not a `HashMap<String, Box<dyn Tool>>`.
//!   Rust's exhaustive matching catches missing handlers at compile time.

use crate::approval_flow::{handle_ask_user, request_approval};
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::file_tracker::FileTracker;
use crate::persistence::Persistence;
use crate::preview;
use crate::providers::ToolCall;
use crate::sub_agent_cache::SubAgentCache;
use crate::sub_agent_dispatch;
use crate::tools;
use crate::trust::{self, ToolApproval, TrustMode};

use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Post-execution recording: emit result event, persist to DB, track progress
/// and file lifecycle. Called after every successful tool execution regardless
/// of execution strategy (parallel, split-batch, or sequential).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn record_tool_result(
    tc: &ToolCall,
    result: &str,
    success: bool,
    full_output: Option<&str>,
    db: &Database,
    session_id: &str,
    max_result_chars: usize,
    project_root: &Path,
    file_tracker: &mut FileTracker,
    sink: &dyn crate::engine::EngineSink,
) -> Result<()> {
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
    crate::progress::track_progress(db, session_id, &tc.function_name, &tc.arguments, result).await;
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

    let mut seen = std::collections::HashSet::new();
    let has_conflict = tool_calls.iter().any(|tc| {
        if !crate::tools::is_mutating_tool(&tc.function_name) {
            return false;
        }
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        if let Some(path) = crate::undo::extract_file_path(&tc.function_name, &args) {
            // If the path is already in the set, we have a conflict
            !seen.insert(path)
        } else {
            false
        }
    });

    !has_conflict
}

/// Execute a single tool call, returning (tool_call_id, result_output, success).
#[tracing::instrument(skip_all, fields(tool = %tc.function_name))]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_one_tool(
    tc: &ToolCall,
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    _session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: TrustMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    sub_agent_cache: &SubAgentCache,
    bg_agents: &std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
) -> (String, String, bool, Option<String>) {
    let (result, success, full_output) = if tc.function_name == "InvokeAgent" {
        // Sub-agents inherit the parent's approval mode.
        match sub_agent_dispatch::execute_sub_agent(
            project_root,
            config,
            db,
            &tc.arguments,
            mode,
            sink,
            cancel.clone(),
            // Sub-agents get a fresh command channel (they auto-approve in all modes)
            &mut mpsc::channel(1).1,
            Some(tools.file_read_cache()),
            sub_agent_cache,
            _session_id,
            bg_agents,
        )
        .await
        {
            Ok(output) => (output, true, None),
            Err(e) => (format!("Error invoking sub-agent: {e}"), false, None),
        }
    } else {
        // Invalidate sub-agent cache on file mutations
        if crate::tools::is_mutating_tool(&tc.function_name) {
            sub_agent_cache.invalidate();
        }
        let streaming = if tc.function_name == "Bash" {
            Some((sink, tc.id.as_str()))
        } else {
            None
        };
        let r = tools
            .execute(&tc.function_name, &tc.arguments, streaming)
            .await;
        (r.output, r.success, r.full_output)
    };

    (tc.id.clone(), result, success, full_output)
}

/// Run multiple tool calls concurrently and store results.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tools_parallel(
    tool_calls: &[ToolCall],
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: TrustMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
    bg_agents: &std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
) -> Result<()> {
    let count = tool_calls.len();
    sink.emit(EngineEvent::Info {
        message: format!("Running {count} tools in parallel..."),
    });

    // Launch all tool calls concurrently
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|tc| {
            execute_one_tool(
                tc,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                sub_agent_cache,
                bg_agents,
            )
        })
        .collect();
    let results = futures_util::future::join_all(futures).await;

    // Emit banner + result together so each tool's output is visually grouped
    for (i, (tc_id, result, success, full_output)) in results.into_iter().enumerate() {
        sink.emit(EngineEvent::ToolCallStart {
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
            db,
            session_id,
            tools.caps.tool_result_chars,
            project_root,
            file_tracker,
            sink,
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tools_split_batch(
    tool_calls: &[ToolCall],
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: TrustMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
    bg_agents: &std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
) -> Result<()> {
    // Partition into parallelizable vs sequential
    let (parallel, sequential): (Vec<_>, Vec<_>) = tool_calls.iter().partition(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            trust::check_tool(&tc.function_name, &args, mode, Some(project_root),),
            ToolApproval::AutoApprove
        )
    });

    // Run parallelizable tools concurrently (if more than one)
    if parallel.len() > 1 {
        sink.emit(EngineEvent::Info {
            message: format!("Running {} tools in parallel...", parallel.len()),
        });

        let futures: Vec<_> = parallel
            .iter()
            .map(|tc| {
                execute_one_tool(
                    tc,
                    project_root,
                    config,
                    db,
                    session_id,
                    tools,
                    mode,
                    sink,
                    cancel.clone(),
                    sub_agent_cache,
                    bg_agents,
                )
            })
            .collect();
        let results = futures_util::future::join_all(futures).await;

        for (j, (tc_id, result, success, full_output)) in results.into_iter().enumerate() {
            sink.emit(EngineEvent::ToolCallStart {
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
                db,
                session_id,
                tools.caps.tool_result_chars,
                project_root,
                file_tracker,
                sink,
            )
            .await?;
        }
    } else {
        // 0–1 parallelizable tools — just run sequentially
        for tc in &parallel {
            let calls = std::slice::from_ref(*tc);
            execute_tools_sequential(
                calls,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                cmd_rx,
                sub_agent_cache,
                file_tracker,
                bg_agents,
            )
            .await?;
        }
    }

    // Run non-parallelizable tools sequentially
    if !sequential.is_empty() {
        let seq_calls: Vec<ToolCall> = sequential.into_iter().cloned().collect();
        execute_tools_sequential(
            &seq_calls,
            project_root,
            config,
            db,
            session_id,
            tools,
            mode,
            sink,
            cancel.clone(),
            cmd_rx,
            sub_agent_cache,
            file_tracker,
            bg_agents,
        )
        .await?;
    }

    Ok(())
}

/// Run tool calls one at a time (when confirmation is needed, or single call).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_tools_sequential(
    tool_calls: &[ToolCall],
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: TrustMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
    bg_agents: &std::sync::Arc<crate::bg_agent::BgAgentRegistry>,
) -> Result<()> {
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
            let answer = handle_ask_user(sink, cmd_rx, &cancel, &parsed_args).await;
            let result = match answer {
                Some(text) if !text.trim().is_empty() => text,
                Some(_) => "User did not provide an answer.".into(),
                None => return Ok(()), // cancelled
            };
            record_tool_result(
                tc,
                &result,
                true,
                None, // AskUser has no full_output
                db,
                session_id,
                tools.caps.tool_result_chars,
                project_root,
                file_tracker,
                sink,
            )
            .await?;
            continue;
        }

        // Pre-flight validation: catch errors before bothering the user
        // with an approval prompt that will inevitably fail.
        if let Some(error) = {
            let cache = tools.file_read_cache();
            let last_writer = tools.last_writer_cache();
            let last_bash = tools.last_bash_cache();
            tools::validate::validate_tool_call(
                &tc.function_name,
                &parsed_args,
                project_root,
                Some(&cache),
                Some(&last_writer),
                Some(&last_bash),
            )
            .await
        } {
            record_tool_result(
                tc,
                &format!("Validation error: {error}"),
                false,
                None,
                db,
                session_id,
                tools.caps.tool_result_chars,
                project_root,
                file_tracker,
                sink,
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
                    &cancel,
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
                    None => {
                        // Cancelled
                        return Ok(());
                    }
                }
            }
        }

        let (_, result, success, full_output) = execute_one_tool(
            tc,
            project_root,
            config,
            db,
            session_id,
            tools,
            mode,
            sink,
            cancel.clone(),
            sub_agent_cache,
            bg_agents,
        )
        .await;
        record_tool_result(
            tc,
            &result,
            success,
            full_output.as_deref(),
            db,
            session_id,
            tools.caps.tool_result_chars,
            project_root,
            file_tracker,
            sink,
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
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_cannot_parallelize_writes() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project")
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
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_can_parallelize_agents() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("InvokeAgent")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project")
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
            Path::new("/test/project")
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
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_is_mutating_tool() {
        assert!(crate::tools::is_mutating_tool("Write"));
        assert!(crate::tools::is_mutating_tool("Edit"));
        assert!(crate::tools::is_mutating_tool("Delete"));
        assert!(crate::tools::is_mutating_tool("Bash"));
        assert!(crate::tools::is_mutating_tool("MemoryWrite"));
        assert!(!crate::tools::is_mutating_tool("Read"));
        assert!(!crate::tools::is_mutating_tool("List"));
        // InvokeAgent is ReadOnly (sub-agents inherit parent's approval mode)
        assert!(!crate::tools::is_mutating_tool("InvokeAgent"));
    }

    #[test]
    fn test_mixed_batch_not_fully_parallelizable() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            TrustMode::Safe,
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_mixed_batch_fully_parallelizable_in_auto() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(can_parallelize(
            &calls,
            TrustMode::Auto,
            Path::new("/test/project")
        ));
    }
}
