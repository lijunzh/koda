//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.

use crate::approval::{self, ApprovalMode, Settings, ToolApproval};
use crate::config::KodaConfig;
use crate::confirm::{self, Confirmation};
use crate::db::{Database, Role};
use crate::display;
use crate::interrupt;
use crate::loop_guard::{self, LoopDetector};
use crate::memory;
use crate::preview;
use crate::providers::{ChatMessage, ImageData, LlmProvider, StreamChunk, ToolCall};
use crate::tools::ToolRegistry;

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;

/// Run inference, executing tool calls until the LLM produces a text response.
#[allow(clippy::too_many_arguments)]
pub async fn inference_loop(
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    system_prompt: &str,
    provider: &dyn LlmProvider,
    tools: &ToolRegistry,
    tool_defs: &[crate::providers::ToolDefinition],
    pending_images: Option<Vec<ImageData>>,
    mode: ApprovalMode,
    settings: &mut Settings,
) -> Result<()> {
    // When native thinking is active, drop ShareReasoning from tool list
    let has_native_thinking = config.model_settings.thinking_budget.is_some()
        || config.model_settings.reasoning_effort.is_some();
    let tool_defs: Vec<crate::providers::ToolDefinition> = if has_native_thinking {
        tool_defs
            .iter()
            .filter(|t| t.name != "ShareReasoning")
            .cloned()
            .collect()
    } else {
        tool_defs.to_vec()
    };
    let tool_defs = &tool_defs;
    let system_tokens = system_prompt.len() / 4 + 100;
    let available = config.max_context_tokens.saturating_sub(system_tokens);
    // Hard cap is configurable per-agent; user can extend it interactively.
    let mut hard_cap = config.max_iterations;
    let mut iteration = 0u32;
    let mut made_tool_calls = false;
    let mut loop_detector = LoopDetector::new();
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;
    let mut total_cache_read_tokens: i64 = 0;
    let mut total_thinking_tokens: i64 = 0;
    let mut total_char_count: usize = 0;
    let loop_start = Instant::now();

    // Pre-build the system message once (avoids re-cloning 4-8KB per iteration)
    let system_message = ChatMessage::text("system", system_prompt);

    loop {
        if iteration >= hard_cap {
            let extra = loop_guard::ask_continue_or_stop(hard_cap, &loop_detector.recent_names());
            if extra == 0 {
                break Ok(());
            }
            hard_cap += extra;
        }

        // Assemble context with sliding window
        let history = db.load_context(session_id, available).await?;
        let mut messages = vec![system_message.clone()];

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

        // Attach pending images to the last user message (first iteration only)
        if iteration == 0
            && let Some(ref imgs) = pending_images
            && !imgs.is_empty()
            && let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user")
        {
            last_user.images = Some(imgs.clone());
        }

        // Track context window usage
        let context_used: usize = messages
            .iter()
            .map(|m| {
                let content_len = m.content.as_deref().map_or(0, |c| c.len());
                let tc_len = m
                    .tool_calls
                    .as_ref()
                    .map_or(0, |tc| serde_json::to_string(tc).map_or(0, |s| s.len()));
                (content_len + tc_len) / 4 + 10
            })
            .sum();
        crate::context::update(context_used, config.max_context_tokens);

        // Stream the response
        let mut spinner = SimpleSpinner::new("\u{1f36f} Thinking...");

        let mut rx = provider
            .chat_stream(&messages, tool_defs, &config.model_settings)
            .await
            .context("LLM inference failed")?;

        // Collect the streamed response with markdown rendering
        let mut md = crate::markdown::MarkdownStreamer::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = crate::providers::TokenUsage::default();
        let mut first_token = true;
        let mut char_count: usize = 0;
        let mut in_think_block = false;
        let mut think_buffer = String::new();
        let mut native_think_buf = String::new();
        let mut response_banner_shown = false;
        let mut thinking_banner_shown = false;
        let mut interrupted = false;

        loop {
            let chunk = tokio::select! {
                c = rx.recv() => c,
                _ = async {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if interrupt::is_interrupted() { break; }
                    }
                } => {
                    // Ctrl+C during receive
                    interrupted = true;
                    None
                }
            };

            if interrupted || interrupt::is_interrupted() {
                spinner.finish_and_clear();
                if !full_text.is_empty() {
                    md.flush();
                }
                println!("\n\x1b[33m\u{26a0} Interrupted\x1b[0m");
                interrupt::clear();
                if !full_text.is_empty() {
                    db.insert_message(
                        session_id,
                        &Role::Assistant,
                        Some(&full_text),
                        None,
                        None,
                        None,
                    )
                    .await?;
                }
                // Store last response for /copy

                return Ok(());
            }

            let Some(chunk) = chunk else { break };

            match chunk {
                StreamChunk::TextDelta(delta) => {
                    if first_token {
                        // Flush any buffered native thinking before showing response
                        if !native_think_buf.is_empty() {
                            spinner.finish_and_clear();
                            display::print_thinking_banner();
                            display::render_thinking_block(&native_think_buf);
                            native_think_buf.clear();
                            thinking_banner_shown = true;
                        }
                        spinner.finish_and_clear();
                        first_token = false;
                    }

                    // If we just finished native thinking, show response banner
                    if thinking_banner_shown && !response_banner_shown && !delta.trim().is_empty() {
                        display::print_response_banner();
                        response_banner_shown = true;
                    }

                    // Detect <think>...</think> tags for reasoning models
                    // (DeepSeek-R1, Qwen QwQ, etc.)
                    full_text.push_str(&delta);
                    char_count += delta.len();
                    think_buffer.push_str(&delta);

                    // Process the buffer for think tags
                    loop {
                        if in_think_block {
                            // Looking for </think>
                            if let Some(end_pos) = think_buffer.find("</think>") {
                                // Render structured thinking block when complete
                                let thinking = &think_buffer[..end_pos];
                                if !thinking.is_empty() {
                                    display::render_thinking_block(thinking);
                                }
                                think_buffer = think_buffer[end_pos + 8..].to_string();
                                in_think_block = false;
                                // Show AGENT RESPONSE banner after thinking ends
                                display::print_response_banner();
                                response_banner_shown = true;
                                continue; // process remaining buffer
                            } else {
                                // Still in think block — just accumulate, don't print yet
                                // (spinner provides progress feedback while we buffer)
                                break;
                            }
                        } else {
                            // Looking for <think>
                            if let Some(start_pos) = think_buffer.find("<think>") {
                                // Render text before <think> tag with markdown
                                let before = &think_buffer[..start_pos];
                                if !before.is_empty() {
                                    if !response_banner_shown && !before.trim().is_empty() {
                                        display::print_response_banner();
                                        response_banner_shown = true;
                                    }
                                    md.push(before);
                                }
                                // Show THINKING banner
                                display::print_thinking_banner();
                                think_buffer = think_buffer[start_pos + 7..].to_string();
                                in_think_block = true;
                                continue; // process remaining buffer
                            } else {
                                // No think tag — check if buffer might contain
                                // a partial "<think" at the end
                                let safe_len =
                                    think_buffer.rfind('<').unwrap_or(think_buffer.len());
                                if safe_len > 0 {
                                    let safe = &think_buffer[..safe_len];
                                    if !response_banner_shown && !safe.trim().is_empty() {
                                        display::print_response_banner();
                                        response_banner_shown = true;
                                    }
                                    md.push(safe);
                                    think_buffer = think_buffer[safe_len..].to_string();
                                }
                                break;
                            }
                        }
                    }
                }
                StreamChunk::ThinkingDelta(delta) => {
                    // Buffer — rendered as a complete block once thinking finishes
                    native_think_buf.push_str(&delta);
                }
                StreamChunk::ToolCalls(tcs) => {
                    if !native_think_buf.is_empty() {
                        spinner.finish_and_clear();
                        display::print_thinking_banner();
                        display::render_thinking_block(&native_think_buf);
                        native_think_buf.clear();
                    }
                    spinner.finish_and_clear();
                    tool_calls = tcs;
                }
                StreamChunk::Done(u) => {
                    // Flush any remaining native thinking (thinking-only turns)
                    if !native_think_buf.is_empty() {
                        spinner.finish_and_clear();
                        display::print_thinking_banner();
                        display::render_thinking_block(&native_think_buf);
                        native_think_buf.clear();
                    }
                    usage = u;
                    break;
                }
            }
        }

        // Flush remaining buffer
        if !think_buffer.is_empty() && !in_think_block {
            if !response_banner_shown && !think_buffer.trim().is_empty() {
                display::print_response_banner();
            }
            md.push(&think_buffer);
        }
        md.flush();

        // If we never showed the AGENT RESPONSE banner (no text or only thinking),
        // and there's non-thinking text, show it now
        // (This is handled inline during streaming above)

        if first_token {
            spinner.finish_and_clear();
        }

        // Log the assistant response
        let content = if full_text.is_empty() {
            None
        } else {
            Some(full_text.as_str())
        };
        let tool_calls_json = if tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tool_calls)?)
        };

        db.insert_message(
            session_id,
            &Role::Assistant,
            content,
            tool_calls_json.as_deref(),
            None,
            Some(&usage),
        )
        .await?;

        // If no tool calls, we already streamed the response — done
        if tool_calls.is_empty() {
            if made_tool_calls && full_text.trim().is_empty() {
                println!(
                    "\n  \x1b[33m\u{26a0} Model produced an empty response after tool use — it may have given up mid-task. Try rephrasing or switching to a more capable model.\x1b[0m"
                );
            }
            total_prompt_tokens += usage.prompt_tokens;
            total_completion_tokens += usage.completion_tokens;
            total_cache_read_tokens += usage.cache_read_tokens;
            total_thinking_tokens += usage.thinking_tokens;
            total_char_count += char_count;

            // Use provider token count, or estimate from char count
            let display_tokens = if total_completion_tokens > 0 {
                total_completion_tokens
            } else {
                (total_char_count / 4) as i64
            };

            let total_elapsed = loop_start.elapsed();
            let total_secs = total_elapsed.as_secs_f64();
            let time_str = format_duration(total_elapsed);
            let rate = if total_secs > 0.0 && display_tokens > 0 {
                display_tokens as f64 / total_secs
            } else {
                0.0
            };

            let ctx = crate::context::format_footer();
            let ctx_part = if ctx.is_empty() {
                String::new()
            } else {
                format!(" \u{00b7} {ctx}")
            };

            // Build enriched footer parts
            let mut parts = Vec::new();
            if total_prompt_tokens > 0 {
                let prompt_k = format_token_count(total_prompt_tokens);
                parts.push(format!("in: {prompt_k}"));
            }
            if display_tokens > 0 {
                parts.push(format!("out: {display_tokens}"));
            }
            parts.push(time_str);
            if display_tokens > 0 {
                parts.push(format!("{rate:.0} t/s"));
            }
            if total_cache_read_tokens > 0 {
                let cache_k = format_token_count(total_cache_read_tokens);
                parts.push(format!("cache: {cache_k} read"));
            }
            if total_thinking_tokens > 0 {
                let think_k = format_token_count(total_thinking_tokens);
                parts.push(format!("thinking: {think_k}"));
            }

            let footer = parts.join(" \u{00b7} ");
            println!("\n\n\x1b[90m{footer}{ctx_part}\x1b[0m\n");

            return Ok(());
        }

        // Accumulate token usage across iterations
        total_prompt_tokens += usage.prompt_tokens;
        total_completion_tokens += usage.completion_tokens;
        total_cache_read_tokens += usage.cache_read_tokens;
        total_thinking_tokens += usage.thinking_tokens;
        total_char_count += char_count;

        made_tool_calls = true;

        // Execute tool calls — parallelize when possible
        if tool_calls.len() > 1
            && can_parallelize(&tool_calls, mode, &settings.approval.allowed_commands)
        {
            execute_tools_parallel(&tool_calls, project_root, config, db, session_id, tools)
                .await?;
        } else {
            execute_tools_sequential(
                &tool_calls,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                settings,
            )
            .await?;
        }

        // Loop detection: same tool+args repeated REPEAT_THRESHOLD times → stop immediately.
        if let Some(fp) = loop_detector.record(&tool_calls) {
            let culprit = fp.split(':').next().unwrap_or("unknown");
            println!(
                "\n  \x1b[31m\u{26a0}  Loop detected: '{culprit}' is repeating with identical arguments.\
                \n  Stopping to avoid wasted work. Rephrase the task or check for ambiguity.\x1b[0m"
            );
            break Ok(());
        }

        iteration += 1;
    }
}

// ── Parallel tool execution ───────────────────────────────────

/// Check if all tool calls in a batch can safely run in parallel.
/// Returns true when NONE of them need user confirmation.
fn can_parallelize(
    tool_calls: &[ToolCall],
    mode: ApprovalMode,
    user_whitelist: &[String],
) -> bool {
    !tool_calls.iter().any(|tc| {
        let args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or_default();
        matches!(
            approval::check_tool(&tc.function_name, &args, mode, user_whitelist),
            ToolApproval::NeedsConfirmation | ToolApproval::Blocked
        )
    })
}

/// Execute a single tool call, returning (tool_call_id, result).
async fn execute_one_tool(
    tc: &ToolCall,
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    tools: &crate::tools::ToolRegistry,
) -> (String, String) {
    let result = if tc.function_name == "InvokeAgent" {
        match execute_sub_agent(project_root, config, db, &tc.arguments).await {
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
async fn execute_tools_parallel(
    tool_calls: &[ToolCall],
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    tools: &crate::tools::ToolRegistry,
) -> Result<()> {
    // Print all tool call banners upfront
    for tc in tool_calls {
        display::print_tool_call(tc, false);
    }

    let count = tool_calls.len();
    println!("\n  \x1b[36m\u{1f43b} Running {count} tools in parallel...\x1b[0m");

    // Launch all tool calls concurrently
    let futures: Vec<_> = tool_calls
        .iter()
        .map(|tc| execute_one_tool(tc, project_root, config, db, tools))
        .collect();
    let results = futures_util::future::join_all(futures).await;

    // Store results and display output (in original order)
    for (i, (tc_id, result)) in results.into_iter().enumerate() {
        display::print_tool_output(&tool_calls[i].function_name, &result);
        db.insert_message(
            session_id,
            &Role::Tool,
            Some(&result),
            None,
            Some(&tc_id),
            None,
        )
        .await?;
    }
    Ok(())
}

/// Run tool calls one at a time (when confirmation is needed, or single call).
#[allow(clippy::too_many_arguments)]
async fn execute_tools_sequential(
    tool_calls: &[ToolCall],
    project_root: &Path,
    config: &KodaConfig,
    db: &Database,
    session_id: &str,
    tools: &crate::tools::ToolRegistry,
    mode: ApprovalMode,
    settings: &mut Settings,
) -> Result<()> {
    for tc in tool_calls {
        // Check for interrupt before each tool
        if interrupt::is_interrupted() {
            println!("\n  \x1b[33m\u{26a0} Interrupted\x1b[0m");
            interrupt::clear();
            return Ok(());
        }

        let parsed_args: serde_json::Value =
            serde_json::from_str(&tc.arguments).unwrap_or_default();

        display::print_tool_call(tc, false);

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
                // Plan mode: show what would happen, don't execute
                let detail = confirm::describe_action(&tc.function_name, &parsed_args);
                let diff_preview =
                    preview::compute(&tc.function_name, &parsed_args, project_root).await;
                println!(
                    "  \x1b[36m\u{1f4cb} [plan] Would execute: {detail}\x1b[0m"
                );
                if let Some(ref preview_text) = diff_preview {
                    for line in preview_text.lines() {
                        println!("  {line}");
                    }
                }
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
                let detail = confirm::describe_action(&tc.function_name, &parsed_args);
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
                    if pattern.is_empty() { None } else { Some(pattern) }
                } else {
                    None
                };

                match confirm::confirm_tool_action(
                    &tc.function_name,
                    &detail,
                    diff_preview.as_deref(),
                    whitelist_hint.as_deref(),
                ) {
                    Confirmation::Approved => {}
                    Confirmation::AlwaysAllow => {
                        // Add to whitelist and persist
                        if let Some(ref pattern) = whitelist_hint {
                            if let Err(e) = settings.add_allowed_command(pattern) {
                                tracing::warn!("Failed to save whitelist: {e}");
                            } else {
                                println!(
                                    "  \x1b[32m\u{2713}\x1b[0m Added '\x1b[1m{pattern}\x1b[0m' to always-allowed commands"
                                );
                            }
                        }
                        // Fall through to execute
                    }
                    Confirmation::Rejected => {
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
                    Confirmation::RejectedWithFeedback(feedback) => {
                        let result =
                            format!("User rejected this action with feedback: {feedback}");
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
                }
            }
        }

        let (_, result) = execute_one_tool(tc, project_root, config, db, tools).await;
        display::print_tool_output(&tc.function_name, &result);

        db.insert_message(
            session_id,
            &Role::Tool,
            Some(&result),
            None,
            Some(&tc.id),
            None,
        )
        .await?;
    }
    Ok(())
}

// ── Sub-agent execution ───────────────────────────────────────

/// Execute a sub-agent in its own isolated event loop.
async fn execute_sub_agent(
    project_root: &Path,
    parent_config: &KodaConfig,
    db: &Database,
    arguments: &str,
) -> Result<String> {
    let args: serde_json::Value = serde_json::from_str(arguments)?;
    let agent_name = args["agent_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'agent_name'"))?;
    let prompt = args["prompt"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'prompt'"))?;
    let session_id = args["session_id"].as_str().map(|s| s.to_string());

    display::print_sub_agent_start(agent_name);

    let sub_config = crate::config::KodaConfig::load(project_root, agent_name)
        .with_context(|| format!("Failed to load sub-agent: {agent_name}"))?;
    let sub_config = sub_config.with_overrides(Some(parent_config.base_url.clone()), None, None);

    let sub_session = match session_id {
        Some(id) => id,
        None => db.create_session(&sub_config.agent_name).await?,
    };

    db.insert_message(&sub_session, &Role::User, Some(prompt), None, None, None)
        .await?;

    let provider = crate::app::create_provider(&sub_config);
    let tools = ToolRegistry::new(project_root.to_path_buf());
    let tool_defs = tools.get_definitions(&sub_config.allowed_tools);
    let semantic_memory = memory::load(project_root)?;
    let system_prompt = build_system_prompt(
        &sub_config.system_prompt,
        &semantic_memory,
        &sub_config.agents_dir,
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

        let mut spinner = SimpleSpinner::new(&format!("  🦥 {agent_name} thinking..."));
        let response = provider
            .chat(&messages, &tool_defs, &sub_config.model_settings)
            .await?;
        spinner.finish_and_clear();

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
            display::print_tool_call(tc, true);
            let result = tools.execute(&tc.function_name, &tc.arguments).await;
            db.insert_message(
                &sub_session,
                &Role::Tool,
                Some(&result.output),
                None,
                Some(&tc.id),
                None,
            )
            .await?;
        }
    }

    println!(
        "  \x1b[33m\u{26a0}  Sub-agent '{agent_name}' hit its iteration limit ({MAX_SUB_AGENT_LIMIT}). Returning partial result.\x1b[0m",
        MAX_SUB_AGENT_LIMIT = loop_guard::MAX_SUB_AGENT_ITERATIONS
    );
    Ok("(sub-agent reached maximum iterations)".to_string())
}

// ── System prompt builder ─────────────────────────────────────

/// Build the full system prompt with semantic memory and available agents.
pub fn build_system_prompt(base_prompt: &str, semantic_memory: &str, agents_dir: &Path) -> String {
    let mut prompt = base_prompt.to_string();

    let available_agents = list_available_agents(agents_dir);
    if !available_agents.is_empty() {
        prompt.push_str("\n\n## Available Sub-Agents\n");
        prompt.push_str(
            "You can delegate tasks to these agents using the InvokeAgent tool. \
             Do NOT invent agent names that are not listed here.\n",
        );
        for name in &available_agents {
            prompt.push_str(&format!("- {name}\n"));
        }
    } else {
        prompt.push_str(
            "\n\nNote: No sub-agents are configured. \
             Do not use the InvokeAgent tool.\n",
        );
    }

    if !semantic_memory.is_empty() {
        prompt.push_str(&format!(
            "\n## Project Memory\n\
             The following are learned facts about this project:\n\
             {semantic_memory}"
        ));
    }

    prompt
}

/// Scan the agents/ directory and return available agent names.
fn list_available_agents(agents_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.strip_suffix(".json").map(|s| s.to_string())
        })
        .collect()
}

// ── Utilities ─────────────────────────────────────────────────

/// Format a token count as human-readable: "1.2k", "432".
fn format_token_count(tokens: i64) -> String {
    if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{tokens}")
    }
}

/// Format a duration as human-readable: "5.2s", "1m 23s".
pub fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let mins = secs / 60;
        let remaining = secs % 60;
        format!("{mins}m {remaining}s")
    }
}

/// Create a terminal spinner.
/// A minimal spinner that uses `\r` to update in place.
/// Immune to terminal resize events (no SIGWINCH handler).
struct SimpleSpinner {
    /// Shared message updated by the spinner updater task.
    /// Accessed via Arc clone, not direct field read.
    #[allow(dead_code)]
    message: std::sync::Arc<std::sync::Mutex<String>>,
    /// Handle to the background tick task.
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl SimpleSpinner {
    fn new(message: &str) -> Self {
        let msg = std::sync::Arc::new(std::sync::Mutex::new(message.to_string()));
        let msg_clone = msg.clone();
        let start = Instant::now();

        // Single task handles both animation and elapsed time updates
        let handle = tokio::spawn(async move {
            let frames = ["⠋", "⠙", "⠸", "⠰", "⠠", "⠆", "⠎", "⠇"];
            let mut i = 0usize;
            loop {
                let frame = frames[i % frames.len()];
                let base = msg_clone.lock().unwrap().clone();
                let elapsed = start.elapsed().as_secs();
                let display = if elapsed > 0 {
                    format!("{base} ({elapsed}s)")
                } else {
                    base
                };
                eprint!("\r\x1b[36m{frame}\x1b[0m {display}\x1b[K");
                let _ = std::io::Write::flush(&mut std::io::stderr());
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            }
        });

        Self {
            message: msg,
            handle: Some(handle),
        }
    }

    fn finish_and_clear(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        eprint!("\r\x1b[K");
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs_f64(0.5)), "0.5s");
        assert_eq!(format_duration(Duration::from_secs_f64(5.23)), "5.2s");
        assert_eq!(format_duration(Duration::from_secs_f64(59.9)), "59.9s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(83)), "1m 23s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(600)), "10m 0s");
    }

    #[test]
    fn test_build_system_prompt_no_agents_no_memory() {
        let dir = TempDir::new().unwrap();
        let result = build_system_prompt("You are helpful.", "", dir.path());
        assert!(result.contains("You are helpful."));
        assert!(result.contains("No sub-agents are configured"));
        assert!(!result.contains("Project Memory"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let dir = TempDir::new().unwrap();
        let result = build_system_prompt("Base prompt.", "Uses Rust. Prefers tokio.", dir.path());
        assert!(result.contains("Base prompt."));
        assert!(result.contains("Project Memory"));
        assert!(result.contains("Uses Rust"));
    }

    #[test]
    fn test_build_system_prompt_with_agents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("reviewer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("planner.json"), "{}").unwrap();

        let result = build_system_prompt("Base.", "", dir.path());
        assert!(result.contains("Available Sub-Agents"));
        assert!(result.contains("reviewer"));
        assert!(result.contains("planner"));
        assert!(!result.contains("No sub-agents"));
    }

    #[test]
    fn test_build_system_prompt_ignores_non_json() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("README.md"), "docs").unwrap();
        std::fs::write(dir.path().join("agent.json"), "{}").unwrap();

        let result = build_system_prompt("Base.", "", dir.path());
        assert!(result.contains("agent"));
        // README.md should not appear as an agent
        assert!(!result.contains("README"));
    }

    #[test]
    fn test_list_available_agents_empty() {
        let dir = TempDir::new().unwrap();
        assert!(list_available_agents(dir.path()).is_empty());
    }

    #[test]
    fn test_list_available_agents_finds_json() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("default.json"), "{}").unwrap();
        std::fs::write(dir.path().join("reviewer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignore").unwrap();

        let agents = list_available_agents(dir.path());
        assert_eq!(agents.len(), 2);
        assert!(agents.contains(&"default".to_string()));
        assert!(agents.contains(&"reviewer".to_string()));
    }

    #[test]
    fn test_list_available_agents_nonexistent_dir() {
        let agents = list_available_agents(Path::new("/nonexistent/path"));
        assert!(agents.is_empty());
    }

    #[test]
    fn test_can_parallelize_read_only() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "Read".into(), arguments: "{}".into() },
            ToolCall { id: "2".into(), function_name: "Grep".into(), arguments: "{}".into() },
            ToolCall { id: "3".into(), function_name: "List".into(), arguments: "{}".into() },
        ];
        assert!(can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_can_parallelize_with_write_is_false() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "Read".into(), arguments: "{}".into() },
            ToolCall { id: "2".into(), function_name: "Write".into(), arguments: "{}".into() },
        ];
        assert!(!can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_can_parallelize_with_unsafe_bash_is_false() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "Read".into(), arguments: "{}".into() },
            ToolCall {
                id: "2".into(),
                function_name: "Bash".into(),
                arguments: r#"{"command": "./deploy.sh"}"#.into(),
            },
        ];
        assert!(!can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_can_parallelize_with_safe_bash() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "Read".into(), arguments: "{}".into() },
            ToolCall {
                id: "2".into(),
                function_name: "Bash".into(),
                arguments: r#"{"command": "cargo test"}"#.into(),
            },
        ];
        assert!(can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }

    #[test]
    fn test_can_parallelize_yolo_always_true() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "Write".into(), arguments: "{}".into() },
            ToolCall { id: "2".into(), function_name: "Delete".into(), arguments: "{}".into() },
        ];
        assert!(can_parallelize(&calls, ApprovalMode::Yolo, &[]));
    }

    #[test]
    fn test_can_parallelize_agents_only() {
        let calls = vec![
            ToolCall { id: "1".into(), function_name: "InvokeAgent".into(), arguments: "{}".into() },
            ToolCall { id: "2".into(), function_name: "InvokeAgent".into(), arguments: "{}".into() },
        ];
        assert!(!can_parallelize(&calls, ApprovalMode::Normal, &[]));
    }
}
