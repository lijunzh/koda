//! Tool execution dispatch — sequential, parallel, and sub-agent.
//!
//! Extracted from inference.rs for clarity. Handles tool call
//! execution, approval flow, and sub-agent delegation.

use crate::approval::{self, ApprovalMode, Settings, ToolApproval};
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::file_tracker::FileTracker;
use crate::loop_guard;
use crate::memory;
use crate::persistence::Persistence;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::{ChatMessage, ToolCall};
use crate::sub_agent_cache::SubAgentCache;
use crate::tools::{self, ToolRegistry};

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

/// Post-edit AST syntax check (#467).
///
/// After a successful Write/Edit, parse the file with tree-sitter and
/// append any syntax errors to the tool result. This gives the LLM
/// immediate feedback to self-correct without user intervention.
///
/// Returns the original result unmodified if:
/// - the file extension isn't supported by koda-ast
/// - the file can't be read (e.g., binary)
/// - the file parses cleanly (no errors)
fn verify_syntax_post_edit(arguments: &str, project_root: &Path, result: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(_) => return result.to_string(),
    };
    let path = match crate::file_tracker::resolve_file_path_from_args(&args, project_root) {
        Some(p) => p,
        None => return result.to_string(),
    };
    match koda_ast::syntax_check(&path) {
        Some(errors) => {
            format!("{result}\n\n{errors}\nFix the syntax errors above before proceeding.")
        }
        None => result.to_string(),
    }
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
    mode: ApprovalMode,
    project_root: &Path,
) -> bool {
    let all_approved = !tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            approval::check_tool(&tc.function_name, &args, mode, Some(project_root)),
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_one_tool(
    tc: &ToolCall,
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    _session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: ApprovalMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    sub_agent_cache: &SubAgentCache,
) -> (String, String, bool) {
    let (result, success) = if tc.function_name == "InvokeAgent" {
        // Sub-agents inherit the parent's approval mode.
        let mut sub_settings = Settings::default();
        match execute_sub_agent(
            project_root,
            config,
            db,
            &tc.arguments,
            mode,
            &mut sub_settings,
            sink,
            cancel.clone(),
            // Sub-agents get a fresh command channel (they auto-approve in all modes)
            &mut mpsc::channel(1).1,
            Some(tools.file_read_cache()),
            sub_agent_cache,
        )
        .await
        {
            Ok(output) => (output, true),
            Err(e) => (format!("Error invoking sub-agent: {e}"), false),
        }
    } else {
        // Invalidate sub-agent cache on file mutations
        if crate::tools::is_mutating_tool(&tc.function_name) {
            sub_agent_cache.invalidate();
        }
        let r = tools.execute(&tc.function_name, &tc.arguments).await;
        (r.output, r.success)
    };

    // Post-edit AST verification (#467): if Write/Edit succeeded, check syntax.
    let result = if success && matches!(tc.function_name.as_str(), "Write" | "Edit") {
        verify_syntax_post_edit(&tc.arguments, project_root, &result)
    } else {
        result
    };

    (tc.id.clone(), result, success)
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
    mode: ApprovalMode,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
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
            )
        })
        .collect();
    let results = futures_util::future::join_all(futures).await;

    // Emit banner + result together so each tool's output is visually grouped
    for (i, (tc_id, result, success)) in results.into_iter().enumerate() {
        sink.emit(EngineEvent::ToolCallStart {
            id: tc_id.clone(),
            name: tool_calls[i].function_name.clone(),
            args: serde_json::from_str(&tool_calls[i].arguments).unwrap_or_default(),
            is_sub_agent: false,
        });
        sink.emit(EngineEvent::ToolCallResult {
            id: tc_id.clone(),
            name: tool_calls[i].function_name.clone(),
            output: result.clone(),
        });
        let stored = truncate_for_history(&result, tools.caps.tool_result_chars);
        db.insert_message(
            session_id,
            &Role::Tool,
            Some(&stored),
            None,
            Some(&tc_id),
            None,
        )
        .await?;
        // Track progress for file mutations and test results
        crate::progress::track_progress(
            db,
            session_id,
            &tool_calls[i].function_name,
            &tool_calls[i].arguments,
            &result,
        )
        .await;
        // File lifecycle tracking (#465, #476)
        let parsed_args: serde_json::Value =
            serde_json::from_str(&tool_calls[i].arguments).unwrap_or_default();
        track_file_lifecycle(
            &tool_calls[i].function_name,
            &parsed_args,
            project_root,
            file_tracker,
            success,
        )
        .await;
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
    mode: ApprovalMode,
    settings: &mut Settings,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
) -> Result<()> {
    // Partition into parallelizable vs sequential
    let (parallel, sequential): (Vec<_>, Vec<_>) = tool_calls.iter().partition(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            approval::check_tool(&tc.function_name, &args, mode, Some(project_root),),
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
                )
            })
            .collect();
        let results = futures_util::future::join_all(futures).await;

        for (j, (tc_id, result, success)) in results.into_iter().enumerate() {
            sink.emit(EngineEvent::ToolCallStart {
                id: tc_id.clone(),
                name: parallel[j].function_name.clone(),
                args: serde_json::from_str(&parallel[j].arguments).unwrap_or_default(),
                is_sub_agent: false,
            });
            sink.emit(EngineEvent::ToolCallResult {
                id: tc_id.clone(),
                name: parallel[j].function_name.clone(),
                output: result.clone(),
            });
            let stored = truncate_for_history(&result, tools.caps.tool_result_chars);
            db.insert_message(
                session_id,
                &Role::Tool,
                Some(&stored),
                None,
                Some(&tc_id),
                None,
            )
            .await?;
            crate::progress::track_progress(
                db,
                session_id,
                &parallel[j].function_name,
                &parallel[j].arguments,
                &result,
            )
            .await;
            // File lifecycle tracking (#465, #476)
            let parsed_args: serde_json::Value =
                serde_json::from_str(&parallel[j].arguments).unwrap_or_default();
            track_file_lifecycle(
                &parallel[j].function_name,
                &parsed_args,
                project_root,
                file_tracker,
                success,
            )
            .await;
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
                settings,
                sink,
                cancel.clone(),
                cmd_rx,
                sub_agent_cache,
                file_tracker,
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
            settings,
            sink,
            cancel.clone(),
            cmd_rx,
            sub_agent_cache,
            file_tracker,
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
    mode: ApprovalMode,
    _settings: &mut Settings,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    sub_agent_cache: &SubAgentCache,
    file_tracker: &mut FileTracker,
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

        // Check approval for this tool call (with file ownership awareness, #465)
        let approval = approval::check_tool_with_tracker(
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

                match request_approval(
                    sink,
                    cmd_rx,
                    &cancel,
                    &tc.function_name,
                    &detail,
                    diff_preview,
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

        let (_, result, success) = execute_one_tool(
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
        )
        .await;
        sink.emit(EngineEvent::ToolCallResult {
            id: tc.id.clone(),
            name: tc.function_name.clone(),
            output: result.clone(),
        });

        let stored = truncate_for_history(&result, tools.caps.tool_result_chars);
        db.insert_message(
            session_id,
            &Role::Tool,
            Some(&stored),
            None,
            Some(&tc.id),
            None,
        )
        .await?;
        // Track progress for file mutations and test results
        crate::progress::track_progress(db, session_id, &tc.function_name, &tc.arguments, &result)
            .await;
        // File lifecycle tracking (#465, #476)
        track_file_lifecycle(
            &tc.function_name,
            &parsed_args,
            project_root,
            file_tracker,
            success,
        )
        .await;
    }
    Ok(())
}

// ── Sub-agent execution ───────────────────────────────────────

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
///
/// Results are cached in `sub_agent_cache` keyed by `(agent_name, prompt_hash)`.
/// On cache hit, returns immediately without any LLM calls.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_sub_agent(
    project_root: &Path,
    parent_config: &KodaConfig,
    db: &Database,
    arguments: &str,
    mode: ApprovalMode,
    _settings: &mut Settings,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
    sub_agent_cache: &SubAgentCache,
) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let agent_name = args["agent_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'agent_name'"))?;
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
    let session_id = args["session_id"].as_str().map(|s| s.to_string());

    // Check result cache (only for stateless calls without a session_id,
    // since session continuations need fresh execution).
    if session_id.is_none()
        && let Some(cached) = sub_agent_cache.get(agent_name, prompt)
    {
        sink.emit(EngineEvent::Info {
            message: format!("  \u{26a1} {agent_name}: cache hit, skipping LLM call"),
        });
        return Ok(cached);
    }

    sink.emit(EngineEvent::SubAgentStart {
        agent_name: agent_name.to_string(),
    });

    let sub_config = crate::config::KodaConfig::load(project_root, agent_name)
        .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;
    // Only inherit parent's base_url if the sub-agent doesn't have its own
    // provider/model explicitly configured (respect agent-level routing).
    let sub_config = if sub_config.provider_type == parent_config.provider_type {
        sub_config.with_overrides(Some(parent_config.base_url.clone()), None, None)
    } else {
        sub_config
    };

    let sub_session = match session_id {
        Some(id) => id,
        None => {
            db.create_session(&sub_config.agent_name, project_root)
                .await?
        }
    };

    db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
        .await?;

    let provider = crate::providers::create_provider(&sub_config);
    let tools = {
        let registry = ToolRegistry::new(project_root.to_path_buf(), sub_config.max_context_tokens);
        match parent_cache {
            Some(cache) => registry.with_shared_cache(cache),
            None => registry,
        }
    };
    let tool_defs = tools.get_definitions(&sub_config.allowed_tools);
    let semantic_memory = memory::load(project_root)?;
    let system_prompt = build_system_prompt(
        &sub_config.system_prompt,
        &semantic_memory,
        &sub_config.agents_dir,
        &tool_defs,
    );

    for _ in 0..loop_guard::MAX_SUB_AGENT_ITERATIONS {
        // Respect parent cancellation (#286)
        if cancel.is_cancelled() {
            return Ok("[cancelled by parent]".to_string());
        }
        let history = db.load_context(&sub_session).await?;
        let mut messages = vec![ChatMessage::text("system", &system_prompt)];
        for msg in &history {
            let tool_calls: Option<Vec<ToolCall>> = msg
                .tool_calls
                .as_deref()
                .and_then(|tc| serde_json::from_str(tc).ok());
            messages.push(ChatMessage {
                role: msg.role.as_str().to_string(),
                content: msg.content.clone(),
                tool_calls,
                tool_call_id: msg.tool_call_id.clone(),
                images: None,
            });
        }

        sink.emit(EngineEvent::SpinnerStart {
            message: format!("  🦥 {agent_name} thinking..."),
        });
        let response = provider
            .chat(&messages, &tool_defs, &sub_config.model_settings)
            .await?;
        sink.emit(EngineEvent::SpinnerStop);

        let tool_calls_json = if response.tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&response.tool_calls)?)
        };

        db.insert_message(
            &sub_session,
            &Role::Assistant,
            response.content.as_deref(),
            tool_calls_json.as_deref(),
            None,
            Some(&response.usage),
        )
        .await?;

        if response.tool_calls.is_empty() {
            let result = response
                .content
                .unwrap_or_else(|| "(no output)".to_string());
            // Cache the result for future identical calls
            sub_agent_cache.put(agent_name, prompt, &result);
            return Ok(result);
        }

        for tc in &response.tool_calls {
            sink.emit(EngineEvent::ToolCallStart {
                id: tc.id.clone(),
                name: tc.function_name.clone(),
                args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                is_sub_agent: true,
            });

            // Sub-agents inherit the parent's approval mode
            let parsed_args: serde_json::Value =
                serde_json::from_str(&tc.arguments).unwrap_or_default();
            let approval =
                approval::check_tool(&tc.function_name, &parsed_args, mode, Some(project_root));

            let output = match approval {
                ToolApproval::AutoApprove => {
                    tools.execute(&tc.function_name, &tc.arguments).await.output
                }
                ToolApproval::Blocked => {
                    let detail = tools::describe_action(&tc.function_name, &parsed_args);
                    let diff_preview =
                        preview::compute(&tc.function_name, &parsed_args, project_root).await;
                    sink.emit(EngineEvent::ActionBlocked {
                        tool_name: tc.function_name.clone(),
                        detail,
                        preview: diff_preview,
                    });
                    "[safe mode] Action blocked.".to_string()
                }
                ToolApproval::NeedsConfirmation => {
                    let detail = tools::describe_action(&tc.function_name, &parsed_args);
                    let diff_preview =
                        preview::compute(&tc.function_name, &parsed_args, project_root).await;
                    match request_approval(
                        sink,
                        cmd_rx,
                        &cancel,
                        &tc.function_name,
                        &detail,
                        diff_preview,
                    )
                    .await
                    {
                        Some(ApprovalDecision::Approve) => {
                            tools.execute(&tc.function_name, &tc.arguments).await.output
                        }
                        Some(ApprovalDecision::Reject) => "[rejected by user]".to_string(),
                        Some(ApprovalDecision::RejectWithFeedback { feedback }) => {
                            format!("[rejected: {feedback}]")
                        }
                        None => "[cancelled]".to_string(),
                    }
                }
            };

            db.insert_message(
                &sub_session,
                &Role::Tool,
                Some(&output),
                None,
                Some(&tc.id),
                None,
            )
            .await?;
        }
    }

    sink.emit(EngineEvent::Warn {
        message: format!(
            "Sub-agent '{agent_name}' hit its iteration limit ({}). Returning partial result.",
            loop_guard::MAX_SUB_AGENT_ITERATIONS
        ),
    });
    Ok("(sub-agent reached maximum iterations)".to_string())
}

pub(crate) async fn request_approval(
    sink: &dyn crate::engine::EngineSink,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    cancel: &CancellationToken,
    tool_name: &str,
    detail: &str,
    preview: Option<crate::preview::DiffPreview>,
) -> Option<ApprovalDecision> {
    let approval_id = uuid::Uuid::new_v4().to_string();
    sink.emit(EngineEvent::ApprovalRequest {
        id: approval_id.clone(),
        tool_name: tool_name.to_string(),
        detail: detail.to_string(),
        preview,
    });

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(EngineCommand::ApprovalResponse { id, decision }) if id == approval_id => {
                    return Some(decision);
                }
                Some(EngineCommand::Interrupt) => {
                    cancel.cancel();
                    return None;
                }
                None => return None,  // channel closed
                _ => continue,        // ignore unrelated commands
            },
            _ = cancel.cancelled() => return None,
        }
    }
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
            ApprovalMode::Confirm,
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_cannot_parallelize_writes() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Write")];
        assert!(!can_parallelize(
            &calls,
            ApprovalMode::Confirm,
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
            ApprovalMode::Confirm,
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_can_parallelize_agents() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("InvokeAgent")];
        assert!(can_parallelize(
            &calls,
            ApprovalMode::Confirm,
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
            ApprovalMode::Auto, // Auto mode would normally allow parallelization
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
            ApprovalMode::Auto,
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
            ApprovalMode::Confirm,
            Path::new("/test/project")
        ));
    }

    #[test]
    fn test_mixed_batch_fully_parallelizable_in_auto() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("Write")];
        assert!(can_parallelize(
            &calls,
            ApprovalMode::Auto,
            Path::new("/test/project")
        ));
    }

    // ── verify_syntax_post_edit tests (#467) ──────────────────────

    #[test]
    fn test_verify_syntax_clean_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()}).to_string();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Wrote ok.rs");
        assert!(!out.contains("syntax error"), "got: {out}");
        assert!(out.starts_with("✓ Wrote ok.rs"));
    }

    #[test]
    fn test_verify_syntax_broken_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bad.rs");
        std::fs::write(&file, "fn main() { let x = ; }").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()}).to_string();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Wrote bad.rs");
        assert!(out.contains("syntax error"), "got: {out}");
        assert!(out.contains("Fix the syntax errors"), "got: {out}");
    }

    #[test]
    fn test_verify_syntax_unsupported_ext() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.csv");
        std::fs::write(&file, "a,b,c").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()}).to_string();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Wrote data.csv");
        assert_eq!(out, "✓ Wrote data.csv");
    }

    #[test]
    fn test_verify_syntax_bad_json_args() {
        let out = verify_syntax_post_edit("not json", Path::new("/tmp"), "ok");
        assert_eq!(out, "ok");
    }

    /// Simulate the realistic flow: start with valid code, apply a bad edit,
    /// verify the hook catches the regression.
    #[test]
    fn test_verify_syntax_edit_introduces_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");

        // Start with valid code
        std::fs::write(&file, "fn hello() { println!(\"hi\"); }\n").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()}).to_string();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Edited lib.rs");
        assert!(
            !out.contains("syntax error"),
            "should be clean before edit: {out}"
        );

        // Simulate a bad edit that breaks the syntax
        std::fs::write(&file, "fn hello( { println!(\"hi\"); }\n").unwrap();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Edited lib.rs");
        assert!(
            out.contains("syntax error"),
            "should catch regression: {out}"
        );
        assert!(
            out.contains("Fix the syntax errors"),
            "should instruct LLM: {out}"
        );
    }

    /// Opposite flow: start with broken code, apply a fix, verify clean result.
    #[test]
    fn test_verify_syntax_edit_fixes_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");

        // Start with broken code
        std::fs::write(&file, "fn hello( { println!(\"hi\"); }\n").unwrap();
        let args = serde_json::json!({"path": file.to_str().unwrap()}).to_string();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Edited lib.rs");
        assert!(
            out.contains("syntax error"),
            "should be broken before fix: {out}"
        );

        // Fix the syntax
        std::fs::write(&file, "fn hello() { println!(\"hi\"); }\n").unwrap();
        let out = verify_syntax_post_edit(&args, dir.path(), "✓ Edited lib.rs");
        assert!(
            !out.contains("syntax error"),
            "should be clean after fix: {out}"
        );
        assert_eq!(out, "✓ Edited lib.rs");
    }
}
