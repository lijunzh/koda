//! LLM inference loop with streaming, tool execution, and sub-agent delegation.
//!
//! Runs the streaming inference → tool execution → re-inference loop
//! until the LLM produces a final text response.
//!
//! ## Loop flow
//!
//! ```text
//! User message
//!   → Build messages array (history + system prompt)
//!   → Stream response from provider
//!   → If tool calls:
//!       → Normalize tool names (handle model quirks)
//!       → Check approval (auto/confirm based on effect)
//!       → Execute tools (parallel when safe)
//!       → Append results to conversation
//!       → Loop (re-inference with tool results)
//!   → If text response:
//!       → Done — return to REPL
//! ```
//!
//! ## Key behaviors
//!
//! - **Streaming**: tokens are emitted as they arrive via `EngineSink`
//! - **Loop guard**: detects repeated identical tool calls and prompts the user
//! - **Auto-compact**: triggers compaction when context usage exceeds threshold
//! - **Microcompact**: ages old tool results between turns
//! - **Sub-agents**: `InvokeAgent` calls spawn a nested inference loop
//! - **Cancellation**: `Ctrl+C` cancels the current inference gracefully
//!
//! ## Design (DESIGN.md)
//!
//! - **Let the model drive (P3)**: The engine is a mechanical loop. It does
//!   not plan, verify, or make decisions — the model does. This loop streams
//!   the response, dispatches tool calls, and feeds results back.
//! - **Rate Limit Retry (P2)**: Exponential backoff for 429 errors. Long
//!   sessions with Opus hit rate limits regularly.

use crate::approval::ApprovalMode;
use crate::config::KodaConfig;
use crate::db::{Database, Role};
use crate::engine::{EngineCommand, EngineEvent, EngineSink};
use crate::file_tracker::FileTracker;
use crate::inference_helpers::{
    AUTO_COMPACT_THRESHOLD, CONTEXT_WARN_THRESHOLD, RATE_LIMIT_MAX_RETRIES, assemble_messages,
    estimate_tokens, is_context_overflow_error, is_rate_limit_error, is_server_error,
    rate_limit_backoff,
};
use crate::loop_guard::LoopDetector;
use crate::persistence::Persistence;
use crate::providers::{
    ChatMessage, ImageData, LlmProvider, StreamChunk, TokenUsage, ToolCall, ToolDefinition,
};
use crate::tool_dispatch::{
    can_parallelize, execute_tools_parallel, execute_tools_sequential, execute_tools_split_batch,
};
use crate::tools::ToolRegistry;

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Inference loop helpers (tightly coupled to inference_loop — live here)
// ---------------------------------------------------------------------------

/// Per-iteration immutable context shared across inference helpers.
///
/// Bundles the parameters that `assemble_context`, `preflight_compact_if_needed`,
/// and `try_overflow_recovery` all share. Built at the top of each loop iteration
/// (since `system_message` and `iteration` change per turn).
struct TurnState<'a> {
    db: &'a Database,
    session_id: &'a str,
    system_message: &'a ChatMessage,
    pending_images: Option<&'a [ImageData]>,
    iteration: u32,
    config: &'a KodaConfig,
    provider: &'a dyn LlmProvider,
    tool_defs: &'a [ToolDefinition],
    sink: &'a dyn EngineSink,
    cancel: &'a CancellationToken,
}

/// Result of collecting a streamed LLM response.
struct StreamResult {
    /// Accumulated text content from the response.
    text: String,
    /// Tool calls requested by the model.
    tool_calls: Vec<ToolCall>,
    /// Results from tools executed eagerly during streaming.
    ///
    /// Contains `(tool_call_id, output, success, full_output)` for each
    /// read-only auto-approved tool that finished before the stream ended.
    /// These tools are skipped during normal dispatch.
    eager_results: Vec<(String, String, bool, Option<String>)>,
    /// Token usage statistics.
    usage: TokenUsage,
    /// Total character count of text deltas.
    char_count: usize,
    /// Whether the stream was interrupted by user cancellation (Ctrl+C).
    interrupted: bool,
    /// Whether the stream ended due to a network error.
    ///
    /// When `true` the partial response MUST be discarded — it is incomplete
    /// and storing it would corrupt the session history on resume.
    network_error: Option<String>,
}

/// Load conversation history, assemble messages with the system prompt,
/// attach pending images (first iteration only), and update context tracking.
///
/// This is the single source of truth for context assembly — called on initial
/// build, after pre-flight compaction, and after overflow recovery.
async fn assemble_context(turn: &TurnState<'_>) -> Result<Vec<ChatMessage>> {
    let history = turn.db.load_context(turn.session_id).await?;

    // Run per-tool context analysis for smarter compaction decisions.
    // Logged at debug level; will be surfaced in `/usage` and used by
    // microcompact (#636 P1) once that lands.
    let analysis = crate::context_analysis::analyze_context(&history);
    if analysis.total > 0 {
        tracing::debug!(
            "Context analysis: {} total, {}% tool results, {}% duplicate reads",
            analysis.total,
            analysis.tool_result_percent(),
            analysis.duplicate_read_percent(),
        );
        for (tool, tokens) in analysis.top_tool_results(3) {
            tracing::debug!("  {tool}: ~{tokens} tokens");
        }
    }

    let mut messages = assemble_messages(turn.system_message, &history);

    // Attach pending images to the last user message (first iteration only)
    if turn.iteration == 0
        && let Some(imgs) = turn.pending_images
        && !imgs.is_empty()
        && let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user")
    {
        last_user.images = Some(imgs.to_vec());
    }

    let context_used = estimate_tokens(&messages);
    crate::context::update(context_used, turn.config.max_context_tokens);
    turn.sink.emit(EngineEvent::ContextUsage {
        used: context_used,
        max: turn.config.max_context_tokens,
    });

    // Warn users when approaching the context limit (headless mode silently
    // drops ContextUsage events, so this Warn is the only signal they get).
    let ctx_pct = crate::context::percentage();
    if (CONTEXT_WARN_THRESHOLD..AUTO_COMPACT_THRESHOLD).contains(&ctx_pct) {
        // Include analysis hints so the user knows *why* context is high.
        let mut warning = format!("Context at {ctx_pct}% — approaching limit.");
        let top = analysis.top_tool_results(2);
        if !top.is_empty() {
            let hogs: Vec<String> = top
                .iter()
                .map(|(name, tokens)| format!("{name} (~{tokens} tok)"))
                .collect();
            warning.push_str(&format!(" Top consumers: {}.", hogs.join(", ")));
        }
        let waste = analysis.total_duplicate_waste();
        if waste > 500 {
            warning.push_str(&format!(" ~{waste} tokens wasted on duplicate file reads."));
        }
        warning.push_str(" Run /compact to free up space.");
        turn.sink.emit(EngineEvent::Warn { message: warning });
    }

    Ok(messages)
}

/// Pre-flight budget check: if context usage exceeds the threshold, compact
/// before sending to the provider. Re-assembles context after successful compaction.
///
/// Returns the (possibly updated) message vec.
async fn preflight_compact_if_needed(
    turn: &TurnState<'_>,
    messages: Vec<ChatMessage>,
) -> Result<Vec<ChatMessage>> {
    let ctx_pct = crate::context::percentage();
    if ctx_pct < AUTO_COMPACT_THRESHOLD {
        return Ok(messages);
    }

    // Circuit breaker: stop wasting API calls after repeated failures
    if crate::compact::is_compact_circuit_broken() {
        tracing::warn!("Pre-flight: context at {ctx_pct}% but circuit breaker tripped — skipping");
        return Ok(messages);
    }

    tracing::warn!("Pre-flight: context at {ctx_pct}%, attempting auto-compact");
    turn.sink.emit(EngineEvent::Info {
        message: format!("\u{1f4e6} Context at {ctx_pct}% \u{2014} compacting before sending..."),
    });

    match crate::compact::compact_session_with_provider(
        turn.db,
        turn.session_id,
        turn.config.max_context_tokens,
        &turn.config.model_settings,
        turn.provider,
    )
    .await
    {
        Ok(Ok(result)) => {
            turn.sink.emit(EngineEvent::Info {
                message: format!(
                    "\u{2705} Compacted {} messages (~{} token summary)",
                    result.deleted, result.summary_tokens
                ),
            });
            assemble_context(turn).await
        }
        Ok(Err(skip)) => {
            tracing::info!("Pre-flight compact skipped: {skip:?}");
            if matches!(skip, crate::compact::CompactSkip::HistoryTooLarge) {
                crate::compact::record_compact_failure();
                turn.sink.emit(EngineEvent::Warn {
                    message: "\u{26a0}\u{fe0f} Context is full but history is too large for \
                              this model to summarize. Start a new session (/session) or \
                              switch to a model with a larger context window."
                        .to_string(),
                });
            }
            Ok(messages)
        }
        Err(e) => {
            tracing::warn!("Pre-flight compact failed: {e:#}");
            let tripped = crate::compact::record_compact_failure();
            let suffix = if tripped {
                " Auto-compact disabled after repeated failures."
            } else {
                " Continuing anyway..."
            };
            turn.sink.emit(EngineEvent::Warn {
                message: format!("Compact failed: {e:#}.{suffix}"),
            });
            Ok(messages)
        }
    }
}

/// Attempt to start a chat stream with exponential backoff on rate limits.
///
/// Returns `Ok(Some(rx))` on success, `Ok(None)` if cancelled during retries,
/// or `Err` for non-retriable failures.
async fn try_with_rate_limit(
    provider: &dyn LlmProvider,
    messages: &[ChatMessage],
    tool_defs: &[ToolDefinition],
    model_settings: &crate::config::ModelSettings,
    cancel: &CancellationToken,
    sink: &dyn EngineSink,
) -> Result<Option<mpsc::Receiver<StreamChunk>>> {
    let mut last_err = None;
    for attempt in 0..RATE_LIMIT_MAX_RETRIES {
        let result = tokio::select! {
            result = provider.chat_stream(messages, tool_defs, model_settings) => result,
            _ = cancel.cancelled() => return Ok(None),
        };
        match result {
            Ok(rx) => return Ok(Some(rx)),
            Err(e) if is_rate_limit_error(&e) && attempt + 1 < RATE_LIMIT_MAX_RETRIES => {
                let delay = rate_limit_backoff(attempt);
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!("\u{23f3} Rate limited. Retrying in {}s...", delay.as_secs()),
                });
                tracing::warn!(
                    "Rate limit (attempt {}/{}): {e:#}",
                    attempt + 1,
                    RATE_LIMIT_MAX_RETRIES
                );
                tokio::time::sleep(delay).await;
                sink.emit(EngineEvent::SpinnerStart {
                    message: format!("Retrying (attempt {})...", attempt + 2),
                });
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Rate limit retries exhausted")))
}

/// Recover from a context overflow error: compact the session, re-assemble
/// context, and retry the provider call once.
///
/// Returns `Ok(Some((rx, messages)))` on success (receiver + updated messages),
/// `Ok(None)` if cancelled during retry, or `Err` if compaction/retry fails.
async fn try_overflow_recovery(
    turn: &TurnState<'_>,
    original_err: anyhow::Error,
) -> Result<Option<(mpsc::Receiver<StreamChunk>, Vec<ChatMessage>)>> {
    turn.sink.emit(EngineEvent::SpinnerStop);
    turn.sink.emit(EngineEvent::Warn {
        message: "\u{26a0}\u{fe0f} Provider rejected request (context overflow). \
             Compacting and retrying..."
            .to_string(),
    });
    tracing::warn!("Context overflow from provider: {original_err:#}");

    match crate::compact::compact_session_with_provider(
        turn.db,
        turn.session_id,
        turn.config.max_context_tokens,
        &turn.config.model_settings,
        turn.provider,
    )
    .await
    {
        Ok(Ok(result)) => {
            turn.sink.emit(EngineEvent::Info {
                message: format!(
                    "\u{2705} Compacted {} messages. Retrying...",
                    result.deleted
                ),
            });
        }
        _ => {
            return Err(original_err)
                .context("LLM inference failed (context overflow, compaction unsuccessful)");
        }
    }

    let messages = assemble_context(turn).await?;

    turn.sink.emit(EngineEvent::SpinnerStart {
        message: "Retrying...".into(),
    });
    let rx = tokio::select! {
        result = turn.provider.chat_stream(&messages, turn.tool_defs, &turn.config.model_settings) => {
            result.context("LLM inference failed after compaction retry")?
        }
        _ = turn.cancel.cancelled() => return Ok(None),
    };
    Ok(Some((rx, messages)))
}

/// Collect a streamed LLM response, executing read-only tools eagerly.
///
/// When a `ToolCallReady` event arrives (Anthropic `content_block_stop`),
/// and the tool is read-only + auto-approved, it executes immediately while
/// subsequent tool call arguments are still being streamed. This overlaps
/// tool execution with LLM generation time — the key latency optimization
/// from Claude Code's `StreamingToolExecutor` pattern.
///
/// Handles thinking → response state transitions, cancellation via `CancellationToken`,
/// and spinner lifecycle. Returns a `StreamResult` — the caller is responsible for
/// persistence and early-return on interruption.
async fn collect_stream(
    rx: &mut mpsc::Receiver<StreamChunk>,
    sink: &dyn EngineSink,
    cancel: &CancellationToken,
    tools: &ToolRegistry,
    mode: ApprovalMode,
    project_root: &Path,
) -> StreamResult {
    let mut full_text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut eager_results: Vec<(String, String, bool, Option<String>)> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut first_token = true;
    let mut char_count: usize = 0;
    let mut native_think_buf = String::new();
    let mut response_banner_shown = false;
    let mut thinking_banner_shown = false;
    let mut interrupted = false;

    loop {
        let chunk = tokio::select! {
            c = rx.recv() => c,
            _ = cancel.cancelled() => {
                interrupted = true;
                None
            }
        };

        if interrupted || cancel.is_cancelled() {
            sink.emit(EngineEvent::SpinnerStop);
            if !full_text.is_empty() {
                sink.emit(EngineEvent::TextDone);
            }
            sink.emit(EngineEvent::Warn {
                message: "Interrupted".into(),
            });
            return StreamResult {
                text: full_text,
                tool_calls,
                eager_results,
                usage,
                char_count,
                interrupted: true,
                network_error: None,
            };
        }

        let Some(chunk) = chunk else { break };

        match chunk {
            StreamChunk::TextDelta(delta) => {
                if first_token {
                    if !native_think_buf.is_empty() {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::ThinkingDone);
                        native_think_buf.clear();
                        thinking_banner_shown = true;
                    }
                    sink.emit(EngineEvent::SpinnerStop);
                    first_token = false;
                }

                if !response_banner_shown && !delta.trim().is_empty() {
                    sink.emit(EngineEvent::ResponseStart);
                    response_banner_shown = true;
                }

                full_text.push_str(&delta);
                char_count += delta.len();
                sink.emit(EngineEvent::TextDelta {
                    text: delta.clone(),
                });
            }
            StreamChunk::ThinkingDelta(delta) => {
                if !thinking_banner_shown {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingStart);
                    thinking_banner_shown = true;
                }
                sink.emit(EngineEvent::ThinkingDelta {
                    text: delta.clone(),
                });
                native_think_buf.push_str(&delta);
            }
            StreamChunk::ToolCallReady(tc) => {
                // A single tool call finished streaming (Anthropic content_block_stop).
                // If it's read-only and auto-approved, execute it now while
                // subsequent tool calls are still being streamed.
                if !native_think_buf.is_empty() {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    native_think_buf.clear();
                }
                let args: serde_json::Value =
                    serde_json::from_str(&tc.arguments).unwrap_or_default();
                let is_read_only = !crate::tools::is_mutating_tool(&tc.function_name);
                let is_auto_approved = !matches!(
                    crate::approval::check_tool(&tc.function_name, &args, mode, Some(project_root),),
                    crate::approval::ToolApproval::NeedsConfirmation
                        | crate::approval::ToolApproval::Blocked
                );

                if is_read_only && is_auto_approved && tc.function_name != "InvokeAgent" {
                    // Execute eagerly — read-only tools are fast (10–50ms),
                    // the channel buffers incoming chunks while we run.
                    tracing::debug!("Eager dispatch: {} (id={})", tc.function_name, tc.id);
                    let r = tools.execute(&tc.function_name, &tc.arguments, None).await;
                    eager_results.push((tc.id.clone(), r.output, r.success, r.full_output));
                }
                // Always add to tool_calls for persistence and normal flow
                tool_calls.push(tc);
            }
            StreamChunk::ToolCalls(tcs) => {
                if !native_think_buf.is_empty() {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    native_think_buf.clear();
                }
                sink.emit(EngineEvent::SpinnerStop);
                // Append — some tool calls may already be in the list from ToolCallReady
                tool_calls.extend(tcs);
            }
            StreamChunk::Done(u) => {
                if !native_think_buf.is_empty() {
                    sink.emit(EngineEvent::SpinnerStop);
                    sink.emit(EngineEvent::ThinkingDone);
                    native_think_buf.clear();
                }
                usage = u;
                break;
            }
            StreamChunk::NetworkError(err) => {
                // Connection dropped mid-stream. Stop rendering and surface a
                // warning. The partial response will be discarded by the caller.
                sink.emit(EngineEvent::SpinnerStop);
                if !full_text.is_empty() {
                    sink.emit(EngineEvent::TextDone);
                }
                sink.emit(EngineEvent::Warn {
                    message: format!("Connection lost mid-stream — turn discarded ({err})"),
                });
                return StreamResult {
                    text: full_text,
                    tool_calls,
                    eager_results,
                    usage,
                    char_count,
                    interrupted: false,
                    network_error: Some(err),
                };
            }
        }
    }

    sink.emit(EngineEvent::TextDone);

    if first_token {
        sink.emit(EngineEvent::SpinnerStop);
    }

    StreamResult {
        text: full_text,
        tool_calls,
        eager_results,
        usage,
        char_count,
        interrupted: false,
        network_error: None,
    }
}

// ---------------------------------------------------------------------------
// Inference loop
// ---------------------------------------------------------------------------

/// All parameters for the inference loop, bundled into a single struct.
pub struct InferenceContext<'a> {
    /// Project root directory.
    pub project_root: &'a Path,
    /// Global configuration.
    pub config: &'a KodaConfig,
    /// Database handle for message persistence.
    pub db: &'a Database,
    /// Current session identifier.
    pub session_id: &'a str,
    /// System prompt for this session.
    pub system_prompt: &'a str,
    /// LLM provider to use.
    pub provider: &'a dyn LlmProvider,
    /// Tool registry with all available tools.
    pub tools: &'a ToolRegistry,
    /// Pre-computed tool definitions sent to the LLM.
    pub tool_defs: &'a [ToolDefinition],
    /// Images attached to the current prompt (consumed on first turn).
    pub pending_images: Option<Vec<ImageData>>,
    /// Current approval mode.
    pub mode: ApprovalMode,
    /// Event sink for streaming output to the client.
    pub sink: &'a dyn EngineSink,
    /// Cancellation token for graceful interruption.
    pub cancel: CancellationToken,
    /// Channel for receiving client commands (approval responses, etc.).
    pub cmd_rx: &'a mut mpsc::Receiver<EngineCommand>,
    /// File lifecycle tracker for ownership-aware approval (#465).
    pub file_tracker: &'a mut FileTracker,
}

/// Run the inference loop: send messages, stream responses, dispatch tool calls.
pub async fn inference_loop(ctx: InferenceContext<'_>) -> Result<()> {
    let InferenceContext {
        project_root,
        config,
        db,
        session_id,
        system_prompt,
        provider,
        tools,
        tool_defs,
        pending_images,
        mode,
        sink,
        cancel,
        cmd_rx,
        file_tracker,
    } = ctx;

    // Hard cap is configurable per-agent; user can extend it interactively.
    let mut hard_cap = config.max_iterations;
    let mut iteration = 0u32;
    let mut made_tool_calls = false;
    let mut retried_empty = false;
    let mut loop_detector = LoopDetector::new();
    let sub_agent_cache = crate::sub_agent_cache::SubAgentCache::new();
    let bg_agents = crate::bg_agent::new_shared();
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;
    let mut total_cache_read_tokens: i64 = 0;
    let mut total_thinking_tokens: i64 = 0;
    let mut total_char_count: usize = 0;
    let loop_start = Instant::now();

    // Pre-build the base system message (avoids re-cloning 4-8KB per iteration)
    let base_system_prompt = system_prompt.to_string();

    // Microcompact: clear old tool results before the first LLM call.
    // Time-based trigger — only fires when the idle gap since the last
    // assistant message exceeds the threshold (not during active tool use).
    if let Ok(Some(mc)) = crate::microcompact::microcompact_session(db, session_id).await {
        sink.emit(EngineEvent::Info {
            message: format!(
                "\u{1f9f9} Microcompact: cleared {} old tool results (~{} tokens)",
                mc.cleared, mc.tokens_saved,
            ),
        });
    }

    loop {
        // Inject completed background agent results as user messages
        for bg_result in bg_agents.drain_completed() {
            let status = if bg_result.success {
                "completed"
            } else {
                "failed"
            };
            let injection = format!(
                "[Background agent '{}' {status}]\n\
                 Original task: {}\n\
                 Result:\n{}",
                bg_result.agent_name, bg_result.prompt, bg_result.output
            );
            sink.emit(EngineEvent::Info {
                message: format!(
                    "  \u{2705} Background agent '{}' {status}",
                    bg_result.agent_name
                ),
            });
            db.insert_message(session_id, &Role::User, Some(&injection), None, None, None)
                .await?;
        }

        if iteration >= hard_cap {
            let recent = loop_detector.recent_names();
            sink.emit(EngineEvent::LoopCapReached {
                cap: hard_cap,
                recent_tools: recent,
            });

            // Wait for client decision via EngineCommand::LoopDecision
            let extra = loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => match cmd {
                        Some(EngineCommand::LoopDecision { action }) => {
                            break action.extra_iterations();
                        }
                        Some(EngineCommand::Interrupt) => {
                            cancel.cancel();
                            break 0;
                        }
                        None => break 0,
                        _ => continue,
                    },
                    _ = cancel.cancelled() => break 0,
                }
            };

            if extra == 0 {
                break Ok(());
            }
            hard_cap += extra;
        }

        // Build system prompt with progress + todo + git context
        let progress = crate::progress::get_progress_summary(db, session_id)
            .await
            .unwrap_or_default();
        let todo_section = crate::tools::todo::get_todo_section(db, session_id).await;
        let git_line = crate::git::git_context(project_root)
            .map(|ctx| format!("\n{ctx}"))
            .unwrap_or_default();
        let system_prompt_full = format!("{base_system_prompt}{progress}{todo_section}{git_line}");
        let system_message = ChatMessage::text("system", &system_prompt_full);

        // Build per-iteration immutable context for helpers
        let turn = TurnState {
            db,
            session_id,
            system_message: &system_message,
            pending_images: pending_images.as_deref(),
            iteration,
            config,
            provider,
            tool_defs,
            sink,
            cancel: &cancel,
        };

        // Assemble context (load history, attach images, track usage)
        let messages = assemble_context(&turn).await?;

        // Pre-flight budget check: if context is critically high, compact first
        let messages = preflight_compact_if_needed(&turn, messages).await?;

        // Stream the response (with rate limit retry)
        sink.emit(EngineEvent::SpinnerStart {
            message: "Thinking...".into(),
        });

        let stream_result = try_with_rate_limit(
            provider,
            &messages,
            tool_defs,
            &config.model_settings,
            &cancel,
            sink,
        )
        .await;

        // Handle cancellation during rate limit retries
        let stream_result = match stream_result {
            Ok(Some(rx)) => Ok(rx),
            Ok(None) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: "Interrupted".into(),
                });
                return Ok(());
            }
            Err(e) => Err(e),
        };

        // Graceful recovery: if the provider returns a context-overflow error,
        // compact and retry once before giving up.
        let mut rx = match stream_result {
            Ok(rx) => rx,
            Err(e) if is_context_overflow_error(&e) => {
                match try_overflow_recovery(&turn, e).await? {
                    Some((rx, _updated)) => rx,
                    None => {
                        sink.emit(EngineEvent::SpinnerStop);
                        sink.emit(EngineEvent::Warn {
                            message: "Interrupted".into(),
                        });
                        return Ok(());
                    }
                }
            }
            Err(e) if is_server_error(&e) => {
                sink.emit(EngineEvent::SpinnerStop);
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Provider returned a server error: {e:#}. \
                         This often means the model can't handle the current \
                         conversation state. Try a different model or start a new session."
                    ),
                });
                return Ok(());
            }
            Err(e) => {
                return Err(e).context("LLM inference failed");
            }
        };

        // Collect the streamed response
        let stream_result = collect_stream(&mut rx, sink, &cancel, tools, mode, project_root).await;

        if stream_result.interrupted {
            if !stream_result.text.is_empty() {
                db.insert_message(
                    session_id,
                    &Role::Assistant,
                    Some(&stream_result.text),
                    None,
                    None,
                    None,
                )
                .await?;
            }
            return Ok(());
        }

        // Network drop: warning already emitted by collect_stream.
        // Discard the partial response — storing it would corrupt the session.
        if stream_result.network_error.is_some() {
            return Ok(());
        }

        let full_text = stream_result.text;
        // Normalize tool names from model output to canonical PascalCase (#548).
        // Models (especially local/small ones via OpenAI-compat APIs) may emit
        // lowercase or snake_case names ("list", "read_file"). This runs for all
        // providers — the canonical fast-path is a single HashMap lookup — and
        // must happen here (not in providers) so dispatch, approval, loop guard,
        // undo, and persistence all see consistent canonical names.
        let tool_calls = crate::tool_normalize::normalize_tool_calls(stream_result.tool_calls);
        let usage = stream_result.usage;
        let char_count = stream_result.char_count;

        // Empty response after tool use — retry once before giving up.
        if tool_calls.is_empty()
            && made_tool_calls
            && full_text.trim().is_empty()
            && usage.stop_reason != "max_tokens"
            && !retried_empty
        {
            retried_empty = true;
            sink.emit(EngineEvent::SpinnerStart {
                message: "Empty response — retrying...".into(),
            });
            continue;
        }

        // Persist the assistant response
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

        let msg_id = db
            .insert_message(
                session_id,
                &Role::Assistant,
                content,
                tool_calls_json.as_deref(),
                None,
                Some(&usage),
            )
            .await?;

        // Mark the message as fully delivered. This distinguishes clean
        // completions from interrupted/in-progress turns on session resume.
        db.mark_message_complete(msg_id).await?;

        // If no tool calls, we already streamed the response — done
        if tool_calls.is_empty() {
            if usage.stop_reason == "max_tokens" {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} hit max_tokens limit — response was truncated. \
                         The context may be too large. Try /compact or start a new session.",
                        config.model,
                    ),
                });
                continue;
            } else if made_tool_calls && full_text.trim().is_empty() {
                sink.emit(EngineEvent::Warn {
                    message: format!(
                        "Model {} produced an empty response after tool use. \
                         Try rephrasing, run /compact, or switch models with /model.",
                        config.model,
                    ),
                });
            }
            total_prompt_tokens += usage.prompt_tokens;
            total_completion_tokens += usage.completion_tokens;
            total_cache_read_tokens += usage.cache_read_tokens;
            total_thinking_tokens += usage.thinking_tokens;
            total_char_count += char_count;

            let display_tokens = if total_completion_tokens > 0 {
                total_completion_tokens
            } else {
                (total_char_count / 4) as i64
            };

            let total_elapsed = loop_start.elapsed();
            let total_secs = total_elapsed.as_secs_f64();
            let rate = if total_secs > 0.0 && display_tokens > 0 {
                display_tokens as f64 / total_secs
            } else {
                0.0
            };

            let context = crate::context::format_footer();

            sink.emit(EngineEvent::Footer {
                prompt_tokens: total_prompt_tokens,
                completion_tokens: total_completion_tokens,
                cache_read_tokens: total_cache_read_tokens,
                thinking_tokens: total_thinking_tokens,
                total_chars: total_char_count,
                elapsed_ms: total_elapsed.as_millis() as u64,
                rate,
                context,
            });

            return Ok(());
        }

        // Accumulate token usage across iterations
        total_prompt_tokens += usage.prompt_tokens;
        total_completion_tokens += usage.completion_tokens;
        total_cache_read_tokens += usage.cache_read_tokens;
        total_thinking_tokens += usage.thinking_tokens;
        total_char_count += char_count;

        made_tool_calls = true;

        // Record results from eagerly-executed tools (dispatched during streaming)
        let eager_ids: std::collections::HashSet<String> = stream_result
            .eager_results
            .iter()
            .map(|(id, _, _, _)| id.clone())
            .collect();

        if !eager_ids.is_empty() {
            tracing::info!(
                "{} tool(s) executed eagerly during streaming",
                eager_ids.len()
            );
            for (tc_id, result, success, full_output) in &stream_result.eager_results {
                // Find the matching ToolCall for metadata
                if let Some(tc) = tool_calls.iter().find(|tc| tc.id == *tc_id) {
                    sink.emit(EngineEvent::ToolCallStart {
                        id: tc_id.clone(),
                        name: tc.function_name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        is_sub_agent: false,
                    });
                    crate::tool_dispatch::record_tool_result(
                        tc,
                        result,
                        *success,
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
            }
        }

        // Filter out eagerly-executed tools from the remaining dispatch
        let remaining_tools: Vec<ToolCall> = tool_calls
            .iter()
            .filter(|tc| !eager_ids.contains(&tc.id))
            .cloned()
            .collect();

        // Execute remaining tool calls — parallelize when possible
        if remaining_tools.len() > 1 && can_parallelize(&remaining_tools, mode, project_root) {
            execute_tools_parallel(
                &remaining_tools,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                &sub_agent_cache,
                file_tracker,
                &bg_agents,
            )
            .await?;
        } else if remaining_tools.len() > 1 {
            execute_tools_split_batch(
                &remaining_tools,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                cmd_rx,
                &sub_agent_cache,
                file_tracker,
                &bg_agents,
            )
            .await?;
        } else if !remaining_tools.is_empty() {
            execute_tools_sequential(
                &remaining_tools,
                project_root,
                config,
                db,
                session_id,
                tools,
                mode,
                sink,
                cancel.clone(),
                cmd_rx,
                &sub_agent_cache,
                file_tracker,
                &bg_agents,
            )
            .await?;
        }

        // Loop detection: same tool+args repeated REPEAT_THRESHOLD times → stop immediately.
        if let Some(fp) = loop_detector.record(&tool_calls) {
            let culprit = fp.split(':').next().unwrap_or("unknown");
            sink.emit(EngineEvent::Warn {
                message: format!(
                    "Loop detected: '{culprit}' is repeating with identical arguments. \
                     Stopping to avoid wasted work. Rephrase the task or check for ambiguity."
                ),
            });
            break Ok(());
        }

        iteration += 1;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalMode;
    use crate::engine::sink::TestSink;
    use crate::providers::{StreamChunk, TokenUsage, ToolCall};
    use tokio::sync::mpsc;

    /// Helper: create a ToolRegistry backed by a temp directory.
    fn test_tools(root: &Path) -> ToolRegistry {
        ToolRegistry::new(root.to_path_buf(), 100_000)
    }

    /// Helper: send chunks into a channel and collect_stream them.
    async fn run_collect(
        chunks: Vec<StreamChunk>,
        cancel: Option<CancellationToken>,
    ) -> StreamResult {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = TestSink::new();
        let cancel = cancel.unwrap_or_else(CancellationToken::new);
        let tmp = tempfile::tempdir().unwrap();
        let tools = test_tools(tmp.path());

        // Send all chunks in a background task.
        tokio::spawn(async move {
            for chunk in chunks {
                let _ = tx.send(chunk).await;
            }
            // tx drops here → stream ends
        });

        collect_stream(
            &mut rx,
            &sink,
            &cancel,
            &tools,
            ApprovalMode::Auto,
            tmp.path(),
        )
        .await
    }

    // ── Text streaming ───────────────────────────────────────────

    #[tokio::test]
    async fn collect_stream_accumulates_text_deltas() {
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("Hello ".into()),
                StreamChunk::TextDelta("world!".into()),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.text, "Hello world!");
        assert!(!result.interrupted);
        assert!(result.network_error.is_none());
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.char_count, 12);
    }

    #[tokio::test]
    async fn collect_stream_empty_stream_returns_empty() {
        let result = run_collect(
            vec![StreamChunk::Done(TokenUsage::default())],
            None,
        )
        .await;

        assert!(result.text.is_empty());
        assert!(!result.interrupted);
        assert!(result.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn collect_stream_preserves_usage_from_done() {
        let usage = TokenUsage {
            prompt_tokens: 42,
            completion_tokens: 17,
            stop_reason: "end_turn".into(),
            ..Default::default()
        };
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("hi".into()),
                StreamChunk::Done(usage),
            ],
            None,
        )
        .await;

        assert_eq!(result.usage.prompt_tokens, 42);
        assert_eq!(result.usage.completion_tokens, 17);
        assert_eq!(result.usage.stop_reason, "end_turn");
    }

    // ── Thinking blocks ──────────────────────────────────────────

    #[tokio::test]
    async fn collect_stream_thinking_then_text() {
        let result = run_collect(
            vec![
                StreamChunk::ThinkingDelta("Let me think...".into()),
                StreamChunk::TextDelta("Answer!".into()),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        // Thinking deltas should NOT appear in the text output.
        assert_eq!(result.text, "Answer!");
    }

    // ── Tool calls ───────────────────────────────────────────────

    #[tokio::test]
    async fn collect_stream_tool_calls_batch() {
        let tc = ToolCall {
            id: "tc_1".into(),
            function_name: "Bash".into(),
            arguments: r#"{"command":"echo hi"}"#.into(),
            thought_signature: None,
        };
        let result = run_collect(
            vec![
                StreamChunk::ToolCalls(vec![tc]),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].function_name, "Bash");
        assert!(result.text.is_empty());
    }

    #[tokio::test]
    async fn collect_stream_eager_executes_read_only_tool() {
        // Read is read-only + auto-approved → should be eagerly executed.
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("hello.txt");
        std::fs::write(&test_file, "file content").unwrap();

        let tc = ToolCall {
            id: "tc_eager".into(),
            function_name: "Read".into(),
            arguments: serde_json::json!({"file_path": test_file.to_string_lossy()}).to_string(),
            thought_signature: None,
        };

        let (tx, mut rx) = mpsc::channel(32);
        let sink = TestSink::new();
        let cancel = CancellationToken::new();
        let tools = test_tools(tmp.path());

        tokio::spawn(async move {
            let _ = tx.send(StreamChunk::ToolCallReady(tc)).await;
            let _ = tx.send(StreamChunk::ToolCalls(vec![])).await;
            let _ = tx.send(StreamChunk::Done(TokenUsage::default())).await;
        });

        let result = collect_stream(
            &mut rx, &sink, &cancel, &tools, ApprovalMode::Auto, tmp.path(),
        )
        .await;

        assert_eq!(result.tool_calls.len(), 1, "tool call should be recorded");
        assert_eq!(result.eager_results.len(), 1, "should have 1 eager result");
        let (id, output, success, _) = &result.eager_results[0];
        assert_eq!(id, "tc_eager");
        assert!(output.contains("file content"), "eager result: {output}");
        assert!(success);
    }

    #[tokio::test]
    async fn collect_stream_does_not_eagerly_execute_mutating_tool() {
        // Write is mutating → should NOT be eagerly executed.
        let tc = ToolCall {
            id: "tc_write".into(),
            function_name: "Write".into(),
            arguments: r#"{"file_path":"/tmp/x","content":"y"}"#.into(),
            thought_signature: None,
        };
        let result = run_collect(
            vec![
                StreamChunk::ToolCallReady(tc),
                StreamChunk::ToolCalls(vec![]),
                StreamChunk::Done(TokenUsage::default()),
            ],
            None,
        )
        .await;

        assert_eq!(result.tool_calls.len(), 1);
        assert!(
            result.eager_results.is_empty(),
            "Write should NOT be eagerly executed"
        );
    }

    // ── Cancellation ─────────────────────────────────────────────

    #[tokio::test]
    async fn collect_stream_cancellation_sets_interrupted() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let (tx, mut rx) = mpsc::channel(32);
        let sink = TestSink::new();
        let tmp = tempfile::tempdir().unwrap();
        let tools = test_tools(tmp.path());

        // Send one delta, then cancel, then try to send more.
        tokio::spawn(async move {
            let _ = tx.send(StreamChunk::TextDelta("partial".into())).await;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
            // This should be ignored after cancel:
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = tx.send(StreamChunk::TextDelta(" ignored".into())).await;
        });

        let result = collect_stream(
            &mut rx, &sink, &cancel, &tools, ApprovalMode::Auto, tmp.path(),
        )
        .await;

        assert!(result.interrupted);
        assert!(result.network_error.is_none());
        // Partial text should be captured up to cancellation.
        assert!(result.text.contains("partial"));
    }

    // ── Network errors ───────────────────────────────────────────

    #[tokio::test]
    async fn collect_stream_network_error_preserves_partial() {
        let result = run_collect(
            vec![
                StreamChunk::TextDelta("partial response".into()),
                StreamChunk::NetworkError("connection reset".into()),
            ],
            None,
        )
        .await;

        assert!(!result.interrupted);
        assert_eq!(
            result.network_error.as_deref(),
            Some("connection reset")
        );
        assert_eq!(result.text, "partial response");
    }

    #[tokio::test]
    async fn collect_stream_network_error_with_no_text() {
        let result = run_collect(
            vec![StreamChunk::NetworkError("timeout".into())],
            None,
        )
        .await;

        assert!(result.text.is_empty());
        assert!(result.network_error.is_some());
    }
}
