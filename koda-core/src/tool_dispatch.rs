//! Tool execution dispatch — sequential, parallel, and sub-agent.
//!
//! Extracted from inference.rs for clarity. Handles tool call
//! execution, approval flow, and sub-agent delegation.

use crate::approval::{self, ApprovalMode, Settings, ToolApproval};
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{ApprovalDecision, EngineCommand, EngineEvent};
use crate::loop_guard;
use crate::memory;
use crate::preview;
use crate::prompt::build_system_prompt;
use crate::providers::{ChatMessage, ToolCall};
use crate::tools::{self, ToolRegistry};

use anyhow::{Context, Result};
use std::path::Path;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Maximum tool result size stored in conversation history.
/// Larger results are truncated to keep context usage bounded.
const MAX_TOOL_RESULT_CHARS: usize = 10_000;

/// Truncate a tool result for storage in conversation history.
fn truncate_for_history(output: &str) -> String {
    if output.len() <= MAX_TOOL_RESULT_CHARS {
        return output.to_string();
    }
    // Find a safe char boundary
    let mut end = MAX_TOOL_RESULT_CHARS;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[...truncated {} chars. Re-read the file if you need the full content.]",
        &output[..end],
        output.len() - end
    )
}

pub(crate) fn can_parallelize(
    tool_calls: &[ToolCall],
    mode: ApprovalMode,
    user_whitelist: &[String],
) -> bool {
    !tool_calls.iter().any(|tc| {
        let args: serde_json::Value = serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            approval::check_tool(&tc.function_name, &args, mode, user_whitelist),
            ToolApproval::NeedsConfirmation | ToolApproval::Blocked
        )
    })
}

/// Execute a single tool call, returning (tool_call_id, result).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_one_tool(
    tc: &ToolCall,
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    _session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: ApprovalMode,
    allowed_commands: &[String],
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
) -> (String, String) {
    let result = if tc.function_name == "InvokeAgent" {
        // Sub-agents inherit the parent's approval mode.
        // We pass a clone of allowed_commands since parallel sub-agents
        // can't mutate the shared settings.
        let mut sub_settings = Settings::default();
        sub_settings.approval.allowed_commands = allowed_commands.to_vec();
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
        )
        .await
        {
            Ok(output) => output,
            Err(e) => format!("Error invoking sub-agent: {e}"),
        }
    } else {
        let r = tools.execute(&tc.function_name, &tc.arguments).await;
        r.output
    };
    (tc.id.clone(), result)
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
    allowed_commands: &[String],
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
) -> Result<()> {
    // Print all tool call banners upfront
    for tc in tool_calls {
        sink.emit(EngineEvent::ToolCallStart {
            id: tc.id.clone(),
            name: tc.function_name.clone(),
            args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
            is_sub_agent: false,
        });
    }

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
                allowed_commands,
                sink,
                cancel.clone(),
            )
        })
        .collect();
    let results = futures_util::future::join_all(futures).await;

    // Store results and display output (in original order)
    for (i, (tc_id, result)) in results.into_iter().enumerate() {
        sink.emit(EngineEvent::ToolCallResult {
            id: tc_id.clone(),
            name: tool_calls[i].function_name.clone(),
            output: result.clone(),
        });
        let stored = truncate_for_history(&result);
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
    settings: &mut Settings,
    sink: &dyn crate::engine::EngineSink,
    cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
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

        // Check approval for this tool call
        let approval = approval::check_tool(
            &tc.function_name,
            &parsed_args,
            mode,
            &settings.approval.allowed_commands,
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
                    Some("[plan mode] Action described but not executed. Switch to normal or yolo mode to execute."),
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

                // For Bash: offer "Always allow" with extracted pattern
                let whitelist_hint = if tc.function_name == "Bash" {
                    let cmd = parsed_args
                        .get("command")
                        .or(parsed_args.get("cmd"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let pattern = approval::extract_whitelist_pattern(cmd);
                    if pattern.is_empty() {
                        None
                    } else {
                        Some(pattern)
                    }
                } else {
                    None
                };

                match request_approval(
                    sink,
                    cmd_rx,
                    &cancel,
                    &tc.function_name,
                    &detail,
                    diff_preview,
                    whitelist_hint.as_deref(),
                )
                .await
                {
                    Some(ApprovalDecision::Approve) => {}
                    Some(ApprovalDecision::AlwaysAllow) => {
                        // Add to whitelist and persist
                        if let Some(ref pattern) = whitelist_hint {
                            if let Err(e) = settings.add_allowed_command(pattern) {
                                tracing::warn!("Failed to save whitelist: {e}");
                            } else {
                                sink.emit(EngineEvent::Info {
                                    message: format!(
                                        "Added '{pattern}' to always-allowed commands"
                                    ),
                                });
                            }
                        }
                        // Fall through to execute
                    }
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

        let (_, result) = execute_one_tool(
            tc,
            project_root,
            config,
            db,
            session_id,
            tools,
            mode,
            &settings.approval.allowed_commands,
            sink,
            cancel.clone(),
        )
        .await;
        sink.emit(EngineEvent::ToolCallResult {
            id: tc.id.clone(),
            name: tc.function_name.clone(),
            output: result.clone(),
        });

        let stored = truncate_for_history(&result);
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
    }
    Ok(())
}

// ── Sub-agent execution ───────────────────────────────────────

/// Execute a sub-agent in its own isolated event loop.
///
/// When `parent_cache` is provided, the sub-agent shares the parent's
/// file-read cache so reads by one agent benefit all others.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_sub_agent(
    project_root: &Path,
    parent_config: &KodaConfig,
    db: &Database,
    arguments: &str,
    mode: ApprovalMode,
    settings: &mut Settings,
    sink: &dyn crate::engine::EngineSink,
    _cancel: CancellationToken,
    cmd_rx: &mut mpsc::Receiver<EngineCommand>,
    parent_cache: Option<crate::tools::FileReadCache>,
) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let agent_name = args["agent_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'agent_name'"))?;
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
    let session_id = args["session_id"].as_str().map(|s| s.to_string());

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
        let registry = ToolRegistry::new(project_root.to_path_buf());
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

    let system_tokens = system_prompt.len() / 4 + 100;
    let available = sub_config.max_context_tokens.saturating_sub(system_tokens);

    for _ in 0..loop_guard::MAX_SUB_AGENT_ITERATIONS {
        let history = db.load_context(&sub_session, available).await?;
        let mut messages = vec![ChatMessage::text("system", &system_prompt)];
        for msg in &history {
            let tool_calls: Option<Vec<ToolCall>> = msg
                .tool_calls
                .as_deref()
                .and_then(|tc| serde_json::from_str(tc).ok());
            messages.push(ChatMessage {
                role: msg.role.clone(),
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
            return Ok(response
                .content
                .unwrap_or_else(|| "(no output)".to_string()));
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
            let approval = approval::check_tool(
                &tc.function_name,
                &parsed_args,
                mode,
                &settings.approval.allowed_commands,
            );

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
                    "[plan mode] Action described but not executed.".to_string()
                }
                ToolApproval::NeedsConfirmation => {
                    let detail = tools::describe_action(&tc.function_name, &parsed_args);
                    let diff_preview =
                        preview::compute(&tc.function_name, &parsed_args, project_root).await;
                    let whitelist_hint = if tc.function_name == "Bash" {
                        let cmd = parsed_args
                            .get("command")
                            .or(parsed_args.get("cmd"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let pattern = approval::extract_whitelist_pattern(cmd);
                        if pattern.is_empty() {
                            None
                        } else {
                            Some(pattern)
                        }
                    } else {
                        None
                    };
                    let sub_cancel = CancellationToken::new();
                    match request_approval(
                        sink,
                        cmd_rx,
                        &sub_cancel,
                        &tc.function_name,
                        &detail,
                        diff_preview,
                        whitelist_hint.as_deref(),
                    )
                    .await
                    {
                        Some(ApprovalDecision::Approve) => {
                            tools.execute(&tc.function_name, &tc.arguments).await.output
                        }
                        Some(ApprovalDecision::AlwaysAllow) => {
                            if let Some(ref pattern) = whitelist_hint {
                                let _ = settings.add_allowed_command(pattern);
                            }
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
    whitelist_hint: Option<&str>,
) -> Option<ApprovalDecision> {
    let approval_id = uuid::Uuid::new_v4().to_string();
    sink.emit(EngineEvent::ApprovalRequest {
        id: approval_id.clone(),
        tool_name: tool_name.to_string(),
        detail: detail.to_string(),
        preview,
        whitelist_hint: whitelist_hint.map(|s| s.to_string()),
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
        assert!(can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_cannot_parallelize_writes() {
        let calls = vec![make_tool_call("Read"), make_tool_call("Write")];
        assert!(!can_parallelize(&calls, ApprovalMode::Normal, &[]));
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
        assert!(!can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_can_parallelize_agents() {
        let calls = vec![make_tool_call("InvokeAgent"), make_tool_call("InvokeAgent")];
        assert!(can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }
}
